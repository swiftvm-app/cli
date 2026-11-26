//! The commands that reach into a guest through its agent: `info`, `wait`, `exec`, `cp`, `ssh`,
//! `forward` and `tunnel`.

use std::io::{IsTerminal, Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use host::{Ended, Error, ErrorCode, ExecOptions, GuestInfo, Host, MachineState, NAME, Result, SetupState};
use serde_json::json;

use crate::ui::{self, Tone, Ui};
use crate::{Arguments, Done, invalid, plain, size};

/// Passes each write on at once: what a command prints is shown as it prints it.
struct Unbuffered<W: Write>(W);

impl<W: Write> Write for Unbuffered<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write_all(bytes)?;
        self.0.flush()?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// The exit status that tells how a command in the guest ended, as a shell tells it.
fn exit_status(ended: &Ended) -> u8 {
    match (ended.status, ended.signal) {
        _ if ended.timed_out => 124,
        (Some(status), _) => status as u8,
        (None, Some(signal)) => 128 + signal as u8,
        (None, None) => 1,
    }
}

fn setup_line(info: &GuestInfo) -> Option<String> {
    let setup = info.setup.as_ref()?;
    let name = setup.name.as_deref().unwrap_or("the setup scripts");
    let state = match setup.state {
        SetupState::Pending => "waiting for the first start to finish",
        SetupState::Running => "running",
        SetupState::Finished => "finished",
        SetupState::Failed => "failed",
        SetupState::Unknown => "unknown",
    };
    Some(match &setup.step {
        Some(step) => format!("{name}: {state}, {step}"),
        None => format!("{name}: {state}"),
    })
}

fn seconds(arguments: &Arguments, name: &str) -> Result<Option<Duration>> {
    Ok(arguments.number::<u64>(name)?.map(Duration::from_secs))
}

pub fn run(command: &str, words: &[String], json: bool) -> Result<Done> {
    match command {
        "info" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let name = arguments.subject("machine")?;
            let host = Host::open()?;
            let machine = host.machine(name)?;
            let guest = match machine.state {
                MachineState::Running => Some(host.guest(name)),
                _ => None,
            };
            if !json {
                let ui = Ui::stdout();
                let mut rows = vec![
                    ("Name", ui.paint(Tone::Bold, &machine.name)),
                    ("State", ui.state(machine.state)),
                    ("Image", plain(&machine.image)),
                    ("Processors", machine.cpus.to_string()),
                    ("Memory", size(machine.memory)),
                    (
                        "Disk",
                        if machine.disk_shared > 0 {
                            format!(
                                "{} of {}, on {} that is shared",
                                size(machine.disk_used),
                                size(machine.disk_size),
                                size(machine.disk_shared)
                            )
                        } else {
                            format!("{} of {}", size(machine.disk_used), size(machine.disk_size))
                        },
                    ),
                    ("Account", plain(&machine.user)),
                ];
                if machine.nested {
                    rows.push(("Nested", "yes: the guest can run machines of its own".into()));
                }
                for (index, share) in machine.shares.iter().enumerate() {
                    rows.push((
                        if index == 0 { "Shared" } else { "" },
                        plain(&crate::share_line(share)),
                    ));
                }
                println!("{}\n{}", ui.paint(Tone::Heading, "Machine"), ui.pairs(&rows));
                rows.clear();
                match &guest {
                    Some(Ok(info)) => {
                        rows.push(("System", plain(info.system.as_deref().unwrap_or("unknown"))));
                        rows.push(("Kernel", plain(&format!("{} ({})", info.kernel, info.arch))));
                        rows.push(("Host name", plain(&info.host_name)));
                        let addresses: Vec<String> = info
                            .addresses
                            .iter()
                            .map(|address| address.address.clone())
                            .collect();
                        rows.push(("Addresses", plain(&addresses.join(", "))));
                        rows.push(("Agent", plain(&info.agent)));
                        rows.push(("Started", if info.booted { "yes" } else { "not yet" }.into()));
                        if let Some(setup) = setup_line(info) {
                            rows.push(("Setup", plain(&setup)));
                        }
                    }
                    Some(Err(error)) => rows.push(("Agent", ui.paint(Tone::Warning, &error.message))),
                    None => {}
                }
                if !rows.is_empty() {
                    println!("\n{}\n{}", ui.paint(Tone::Heading, "Guest"), ui.pairs(&rows));
                }
            }
            let (guest, guest_error) = match guest {
                Some(Ok(info)) => (Some(info), None),
                Some(Err(error)) => (None, Some(error)),
                None => (None, None),
            };
            Ok(json!({ "machine": machine, "guest": guest, "guest_error": guest_error }).into())
        }
        "wait" => {
            let arguments = Arguments::parse(words, &["timeout"], &[])?;
            let name = arguments.subject("machine")?;
            let timeout = seconds(&arguments, "timeout")?;
            let mut shown = None;
            let mut reported = None;
            let info = Host::open()?.wait(name, timeout, &mut |info| {
                if json {
                    let now = (info.booted, info.setup.clone());
                    if reported.as_ref() != Some(&now) {
                        crate::json_note(
                            json!({ "progress": "guest", "booted": info.booted, "setup": info.setup }),
                        );
                        reported = Some(now);
                    }
                    return;
                }
                let line = setup_line(info);
                if line.is_some() && line != shown {
                    eprintln!(
                        "{}",
                        Ui::stderr().paint(Tone::Bold, line.as_deref().unwrap_or_default())
                    );
                    shown = line;
                }
            })?;
            ui::success(json, format!("{name} is ready."));
            Ok(json!({ "guest": info }).into())
        }
        "exec" => {
            let arguments = Arguments::parse(words, &["user", "cwd", "env", "timeout"], &[])?;
            let name = arguments.subject("machine")?;
            if arguments.rest.is_empty() {
                return Err(invalid(format!(
                    "Say which command to run: {NAME} exec <machine> -- <command>."
                )));
            }
            let mut environment = std::collections::BTreeMap::new();
            for pair in arguments.values("env") {
                let Some((key, value)) = pair.split_once('=') else {
                    return Err(invalid(format!("--env takes name=value, not \"{pair}\".")));
                };
                environment.insert(key.to_string(), value.to_string());
            }
            let options = ExecOptions {
                arguments: arguments.rest.clone(),
                user: arguments.value("user"),
                directory: arguments.value("cwd"),
                environment,
                timeout: seconds(&arguments, "timeout")?,
            };
            // A terminal isn't passed on: the command would wait for input that nobody knows
            // it wants.
            let input =
                (!std::io::stdin().is_terminal()).then(|| Box::new(std::io::stdin()) as Box<dyn Read + Send>);
            let host = Host::open()?;
            if json {
                let (mut output, mut errors) = (Vec::new(), Vec::new());
                let ended = host.exec(name, options, input, &mut output, &mut errors)?;
                let result = json!({
                    "exit": ended,
                    "output": String::from_utf8_lossy(&output),
                    "errors": String::from_utf8_lossy(&errors),
                });
                return Ok(Done {
                    result,
                    status: exit_status(&ended),
                });
            }
            let mut output = Unbuffered(std::io::stdout().lock());
            let mut errors = Unbuffered(std::io::stderr());
            let ended = host.exec(name, options, input, &mut output, &mut errors)?;
            if ended.timed_out {
                ui::error("The command ran out of time, and was ended.");
            }
            if let Some(error) = &ended.error {
                ui::error(&error.message);
            }
            Ok(Done {
                result: serde_json::Value::Null,
                status: exit_status(&ended),
            })
        }
        "cp" => {
            let arguments = Arguments::parse(words, &["user"], &[])?;
            let [from, to] = arguments.positional.as_slice() else {
                return Err(invalid("Say what to copy and where to: cp <from> <to>."));
            };
            let user = arguments.value("user");
            let host = Host::open()?;
            match (in_guest(from), in_guest(to)) {
                (None, Some((name, path))) => {
                    host.copy_to_guest(name, Path::new(from), path, user.as_deref())?
                }
                (Some((name, path)), None) => {
                    host.copy_from_guest(name, path, Path::new(to), user.as_deref())?
                }
                _ => {
                    return Err(invalid(
                        "One side of a copy is in a guest, written <machine>:<path>, and the other on this computer.",
                    ));
                }
            }
            ui::success(json, format!("Copied {from} to {to}."));
            Ok(json!({ "copied": { "from": from, "to": to } }).into())
        }
        "ssh" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let name = arguments.subject("machine")?;
            ssh(&Host::open()?, name, &arguments.rest)
        }
        "forward" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let Some((name, ports)) = arguments.positional.split_first() else {
                return Err(invalid("Say which machine."));
            };
            if ports.is_empty() {
                return Err(invalid("Say which ports: forward <machine> 8080:80."));
            }
            forward(name, ports, json)
        }
        "tunnel" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let [name, port] = arguments.positional.as_slice() else {
                return Err(invalid(
                    "Say which machine and which port: tunnel <machine> <port>.",
                ));
            };
            let port = port
                .parse()
                .map_err(|_| invalid(format!("\"{port}\" isn't a port.")))?;
            let stream = Host::open()?.tunnel(name, port)?;
            tunnel(stream);
            Ok(serde_json::Value::Null.into())
        }
        _ => unreachable!("only the guest's commands are passed here"),
    }
}

/// The machine and the path that `word` names, if it names a place in a guest:
/// `<machine>:<path>`. A path that starts with `/` or `.` is this computer's.
fn in_guest(word: &str) -> Option<(&str, &str)> {
    if word.starts_with(['/', '.']) {
        return None;
    }
    let (name, path) = word.split_once(':')?;
    let is_name = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    // Nothing after the colon is the account's home folder.
    is_name.then_some((name, if path.is_empty() { "." } else { path }))
}

/// Relays this program's input and output to and from `stream` until the guest's side ends.
fn tunnel(stream: std::os::unix::net::UnixStream) {
    if let Ok(mut sending) = stream.try_clone() {
        std::thread::spawn(move || {
            _ = std::io::copy(&mut std::io::stdin().lock(), &mut sending);
            _ = sending.shutdown(Shutdown::Write);
        });
    }
    let mut receiving = stream;
    _ = std::io::copy(&mut receiving, &mut Unbuffered(std::io::stdout().lock()));
}

fn forward(name: &str, ports: &[String], json: bool) -> Result<Done> {
    let host = Arc::new(Host::open()?);
    host.machine(name)?;
    let mut listeners = Vec::new();
    for pair in ports {
        let (local, remote) = pair.split_once(':').unwrap_or((pair, pair));
        let parse = |port: &str| {
            port.parse::<u16>()
                .map_err(|_| invalid(format!("\"{pair}\" isn't a port, or two ports as 8080:80.")))
        };
        let (local, remote) = (parse(local)?, parse(remote)?);
        let listener = TcpListener::bind(("127.0.0.1", local)).map_err(|error| {
            Error::new(
                ErrorCode::Io,
                format!("Couldn't listen on port {local}: {error}."),
            )
        })?;
        // Port 0 is any free port: say which one it is.
        let local = listener
            .local_addr()
            .map_err(|error| {
                Error::new(
                    ErrorCode::Io,
                    format!("Couldn't listen on port {local}: {error}."),
                )
            })?
            .port();
        listeners.push((listener, local, remote));
    }
    if json {
        // A program that runs this learns that it can connect, and to what.
        let forwarding: Vec<_> = listeners
            .iter()
            .map(|(_, local, remote)| json!({ "local": local, "guest": remote }))
            .collect();
        println!("{}", json!({ "forwarding": forwarding }));
        _ = std::io::stdout().flush();
    } else {
        let ui = Ui::stderr();
        for (_, local, remote) in &listeners {
            eprintln!(
                "Forwarding {} to port {remote} of {}.",
                ui.paint(Tone::Bold, &format!("localhost:{local}")),
                plain(name)
            );
        }
        eprintln!("{}", ui.paint(Tone::Muted, "Control-C stops."));
    }
    let mut threads = Vec::new();
    for (listener, _, remote) in listeners {
        let (host, name) = (host.clone(), name.to_string());
        threads.push(std::thread::spawn(move || {
            for connection in listener.incoming().flatten() {
                let (host, name) = (host.clone(), name.clone());
                std::thread::spawn(move || match host.tunnel(&name, remote) {
                    Ok(stream) => host::relay(connection, stream),
                    Err(error) if json => crate::json_note(json!({ "error": error })),
                    Err(error) => ui::error(&error.message),
                });
            }
        }));
    }
    for thread in threads {
        _ = thread.join();
    }
    Ok(serde_json::Value::Null.into())
}

/// Quotes `text` for a shell.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Adds the key to the account's authorized keys, unless it is there.
const AUTHORIZE: &str = r#"umask 077; mkdir -p "$HOME/.ssh" && touch "$HOME/.ssh/authorized_keys" && { grep -qxF "$1" "$HOME/.ssh/authorized_keys" || printf '%s\n' "$1" >> "$HOME/.ssh/authorized_keys"; }"#;

/// The key that this computer signs in to guests with, made the first time it is needed.
fn ssh_key(store: &Path) -> Result<PathBuf> {
    let folder = store.join("ssh");
    let key = folder.join("id_ed25519");
    if key.exists() {
        return Ok(key);
    }
    std::fs::create_dir_all(&folder).map_err(|error| {
        Error::new(
            ErrorCode::Io,
            format!("Couldn't create {}: {error}.", folder.display()),
        )
    })?;
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", NAME, "-f"])
        .arg(&key)
        .stdin(std::process::Stdio::null())
        .status();
    match made {
        Ok(status) if status.success() => Ok(key),
        _ => Err(Error::new(
            ErrorCode::Io,
            format!(
                "Couldn't make a key to sign in with, at {}, with ssh-keygen.",
                key.display()
            ),
        )),
    }
}

/// Signs in to the guest with the system's ssh, which reaches it through `tunnel`. Only returns
/// when ssh can't be started.
fn ssh(host: &Host, name: &str, extra: &[String]) -> Result<Done> {
    let machine = host.machine(name)?;
    if machine.state != MachineState::Running {
        return Err(Error::new(
            ErrorCode::NotRunning,
            format!("\"{name}\" isn't running."),
        ));
    }
    let store = host.store().root();
    let key = ssh_key(store)?;
    let public = std::fs::read_to_string(key.with_extension("pub"))
        .map_err(|error| Error::new(ErrorCode::Io, format!("Couldn't read the key: {error}.")))?;
    let options = ExecOptions {
        arguments: ["sh", "-c", AUTHORIZE, "sh", public.trim()]
            .map(String::from)
            .to_vec(),
        ..ExecOptions::default()
    };
    let mut errors = Vec::new();
    let ended = host.exec(name, options, None, &mut std::io::sink(), &mut errors)?;
    if !ended.success() {
        return Err(Error::new(
            ErrorCode::Guest,
            format!(
                "Couldn't let this computer's key in: {}",
                String::from_utf8_lossy(&errors).trim()
            ),
        ));
    }
    let program = std::env::current_exe()
        .map_err(|error| Error::new(ErrorCode::Io, format!("Couldn't find this program: {error}.")))?;
    let proxy = format!(
        "{} tunnel {} %p",
        quoted(&program.to_string_lossy()),
        quoted(name)
    );
    let error = std::process::Command::new("ssh")
        .arg("-i")
        .arg(&key)
        .args(["-o", "IdentitiesOnly=yes"])
        .arg("-o")
        .arg(format!("ProxyCommand={proxy}"))
        .arg("-o")
        .arg(format!("HostKeyAlias={NAME}-{}", machine.id))
        .arg("-o")
        // Quoted: ssh takes several files there, apart at spaces, and the store's path may have one.
        .arg(format!(
            "UserKnownHostsFile=\"{}\"",
            store.join("ssh/known_hosts").display()
        ))
        .args(["-o", "StrictHostKeyChecking=accept-new", "-o", "LogLevel=ERROR"])
        // The account apart from the destination, and nothing after `--` taken for an option.
        .arg("-l")
        .arg(&machine.user)
        .arg("--")
        .arg(name)
        .args(extra)
        .exec();
    Err(Error::new(ErrorCode::Io, format!("Couldn't start ssh: {error}.")))
}
