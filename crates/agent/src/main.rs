//! The guest's agent. It runs in the guest as root, started by systemd, and does there what the
//! host asks for over the machine's socket device: runs commands, copies files, connects to
//! ports, and says how the guest is doing.
//!
//! ```text
//! agent                 listen on the socket device, as the guest's service does
//! agent listen <path>   listen on a Unix socket instead, to try the agent outside a guest
//! agent pack <path>     write what is at <path> to the standard output, for `cp`
//! agent unpack <path>   put what the standard input holds at <path>, for `cp`
//! agent identity        renew the machine ID of a machine made from an image, early in its start
//! ```

mod account;
mod freeze;
mod identity;
mod job;
mod mount;
mod serve;
mod system;
#[cfg(target_os = "linux")]
mod vsock;

use std::io::{BufWriter, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use serve::Agent;

fn main() -> ExitCode {
    let arguments: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mode = arguments.first().and_then(|mode| mode.to_str());
    match (mode, arguments.get(1).map(Path::new)) {
        (None, _) => serve_socket_device(),
        (Some("listen"), Some(path)) if arguments.len() == 2 => listen(path),
        (Some("pack"), Some(path)) if arguments.len() == 2 => {
            let mut output = BufWriter::with_capacity(64 * 1024, std::io::stdout().lock());
            finish(
                proto::copy::pack(path, &mut output).and_then(|()| output.flush()),
                path,
            )
        }
        (Some("identity"), None) => match identity::renew(Path::new("/")) {
            Ok(outcome) => {
                eprintln!("{outcome:?}");
                if outcome == identity::Outcome::Renewed {
                    // The journal had started with the machine ID that was there before, and
                    // would go on writing under it. Queued, as this runs before most of the start.
                    _ = std::process::Command::new("systemctl")
                        .args(["restart", "--no-block", "systemd-journald.service"])
                        .status();
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Couldn't renew the machine ID: {error}");
                ExitCode::FAILURE
            }
        },
        (Some("unpack"), Some(path)) if arguments.len() == 2 => {
            // The host is trusted to name what it sends.
            finish(proto::copy::unpack(std::io::stdin().lock(), path, None), path)
        }
        _ => {
            eprintln!("usage: agent [listen <socket> | pack <path> | unpack <path> | identity]");
            ExitCode::from(2)
        }
    }
}

fn finish(result: std::io::Result<()>, path: &Path) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {error}", path.display());
            ExitCode::FAILURE
        }
    }
}

/// Serves connections on a Unix socket at `path`, from whoever can open it.
fn listen(path: &Path) -> ExitCode {
    _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("Couldn't listen on {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let agent = Arc::new(Agent::new());
    for stream in listener.incoming().flatten() {
        let agent = agent.clone();
        std::thread::spawn(move || agent.handle(stream));
    }
    ExitCode::SUCCESS
}

#[cfg(target_os = "linux")]
fn serve_socket_device() -> ExitCode {
    use std::time::Duration;

    // An agent that ended while the file system was frozen left it so.
    freeze::thaw();
    // Off the way of the host's first connections, which may be what mounts them anyway.
    std::thread::spawn(mount::restore);
    let agent = Arc::new(Agent::new());
    // The socket device's driver may not be there yet this early in the system's start.
    let mut said = false;
    let listener = loop {
        match vsock::Listener::bind(proto::agent::AGENT_PORT) {
            Ok(listener) => break listener,
            Err(error) => {
                if !said {
                    eprintln!("Waiting for the socket device: {error}");
                    said = true;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    };
    eprintln!(
        "Listening on port {} of the socket device.",
        proto::agent::AGENT_PORT
    );
    loop {
        match listener.accept() {
            Ok((stream, cid)) if cid == libc::VMADDR_CID_HOST => {
                let agent = agent.clone();
                std::thread::spawn(move || agent.handle(stream));
            }
            // A program in the guest, through the socket device's loopback: it would have the
            // agent run what it likes as root.
            Ok((_, cid)) => eprintln!("Refused a connection from {cid}, which isn't the host."),
            // Out of descriptors, for one: that passes.
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn serve_socket_device() -> ExitCode {
    eprintln!(
        "The agent listens on a socket device only in Linux. `agent listen <path>` listens on a Unix socket."
    );
    ExitCode::from(2)
}
