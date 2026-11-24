//! The other side of a machine's process: starts one, and talks to it.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use proto::{Input, NAME, Request, Response, ScreenSize, Screenshot, Status};

use crate::lock::Lock;
use crate::process::{self, PROCESS_FILE, ProcessFile, STOP_TIMEOUT};
use crate::store::LOCK;
use crate::{Context, Error, ErrorCode, Result};

/// How long a process that has just started gets to listen, and one that was told to stop gets
/// to end.
const PATIENCE: Duration = Duration::from_secs(5);
/// How long cutting a machine's power may take on top of what its guest was given to shut down.
const STOP_MARGIN: Duration = Duration::from_secs(90);
/// The longest a guest is given to shut down, whatever is asked for: no time limit that a
/// system call takes is longer.
const LONGEST_STOP: Duration = Duration::from_secs(u32::MAX as u64);

/// Whether the machine in `directory` has a process, which holds its lock for as long as it lives.
pub(crate) fn is_running(directory: &Path) -> bool {
    Lock::is_held(&directory.join(LOCK))
}

/// What the machine's process wrote down about itself. Only to be believed while it is running:
/// a process that was killed leaves the file behind.
pub(crate) fn process_file(directory: &Path) -> Option<ProcessFile> {
    serde_json::from_slice(&std::fs::read(directory.join(PROCESS_FILE)).ok()?).ok()
}

/// Starts a process for the machine in `directory` and waits until the machine has started. The
/// process is left to itself: it outlives this program.
pub(crate) fn start(program: &Path, directory: &Path) -> Result<()> {
    let log_path = directory.join("machine.log");
    let log = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&log_path)
        .at("open", &log_path)?;
    let mut command = Command::new(program);
    command
        .arg(process::ARGUMENT)
        .arg(directory)
        // Not in the folder this program was started in, which it would keep busy.
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    // Safety: between fork and exec, these are among the calls that are allowed.
    unsafe {
        command.pre_exec(|| {
            // A session of its own, so that neither closing the terminal nor a Control-C meant
            // for this program ends it.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // Whatever else this program has open stays behind: a pipe that someone waits for
            // the end of, or a port the embedding app listens on, would live as long as the
            // machine does. Closed when the new program starts rather than now, which keeps
            // the pipe that reports a failure to start it.
            let limit = match libc::sysconf(libc::_SC_OPEN_MAX) {
                limit if limit > 0 => limit.min(1 << 16) as libc::c_int,
                _ => 1024,
            };
            for descriptor in 3..limit {
                libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    let mut child = command.spawn().at("start", program)?;
    let mut answers = child.stdout.take().expect("the output is a pipe");
    let answer = proto::receive::<Response>(&mut answers);
    // Someone has to wait for the process, or the system keeps what is left of it when it ends
    // for as long as this program runs.
    std::thread::spawn(move || _ = child.wait());
    match answer {
        Ok(Some(response)) => response.into_result().map(|_| ()),
        _ => Err(Error::new(
            ErrorCode::Engine,
            format!(
                "The machine's process ended before the machine had started. {} may say why.",
                log_path.display()
            ),
        )),
    }
}

fn not_answering(directory: &Path, why: impl std::fmt::Display) -> Error {
    let process =
        process_file(directory).map_or(String::new(), |process| format!(" (process {})", process.pid));
    Error::new(
        ErrorCode::Io,
        format!("The machine's process{process} doesn't answer: {why}."),
    )
}

/// Connects to the machine's process and says hello. Returns the version of the protocol that
/// both speak.
fn connect(directory: &Path) -> Result<(UnixStream, u32)> {
    // A process that has only just started holds the lock a moment before it listens.
    let deadline = Instant::now() + PATIENCE;
    let mut stream = loop {
        let attempt = process_file(directory)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
            .and_then(|process| UnixStream::connect(process.socket));
        match attempt {
            Ok(stream) => break stream,
            Err(_) if !is_running(directory) => {
                return Err(Error::new(ErrorCode::NotRunning, "The machine isn't running."));
            }
            Err(error) if Instant::now() >= deadline => return Err(not_answering(directory, error)),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    // A process that listens but has stopped answering doesn't keep this one waiting for good.
    stream
        .set_read_timeout(Some(PATIENCE))
        .context("set a time limit")?;
    let version = proto::greet(&mut stream, env!("CARGO_PKG_VERSION"))?;
    Ok((stream, version))
}

/// Sends `request` and returns the answer, which it waits for `patience` at most.
fn ask(directory: &Path, request: &Request, patience: Duration) -> Result<(UnixStream, Option<Status>)> {
    let (stream, response) = ask_for(directory, request, patience)?;
    Ok((stream, response.status))
}

/// The same, with the whole answer.
fn ask_for(directory: &Path, request: &Request, patience: Duration) -> Result<(UnixStream, Response)> {
    let (mut stream, version) = connect(directory)?;
    if matches!(request, Request::Screenshot | Request::Input { .. }) && version < SCREEN_PROTOCOL {
        return Err(Error::new(
            ErrorCode::Protocol,
            format!(
                "The machine was started by an older build of {NAME}, which can't reach its screen. Stop it and start it again."
            ),
        ));
    }
    stream
        .set_read_timeout(Some(patience))
        .context("set a time limit")?;
    proto::send(&mut stream, request).map_err(|error| not_answering(directory, error))?;
    let response = proto::receive::<Response>(&mut stream)
        .map_err(|error| not_answering(directory, error))?
        .ok_or_else(|| not_answering(directory, "it hung up"))?;
    stream.set_read_timeout(None).context("set a time limit")?;
    Ok((stream, response.checked()?))
}

/// The version of the control protocol that has the screen's requests.
const SCREEN_PROTOCOL: u32 = 2;

/// How long the screen gets to answer: the first time, the guest's VNC server is connected to
/// through the agent, which may be replaced by a newer one on the way.
const SCREEN_PATIENCE: Duration = Duration::from_secs(60);

/// A picture of the guest's screen, as a PNG.
pub(crate) fn screenshot(directory: &Path) -> Result<(Screenshot, Vec<u8>)> {
    let (mut stream, response) = ask_for(directory, &Request::Screenshot, SCREEN_PATIENCE)?;
    let shot = response
        .screenshot
        .ok_or_else(|| not_answering(directory, "its answer has no picture"))?;
    stream
        .set_read_timeout(Some(PATIENCE))
        .context("set a time limit")?;
    let mut png = Vec::new();
    (&mut stream)
        .take(shot.size)
        .read_to_end(&mut png)
        .map_err(|error| not_answering(directory, error))?;
    if png.len() as u64 != shot.size {
        return Err(not_answering(directory, "the picture was cut short"));
    }
    Ok((shot, png))
}

/// Sends events to the guest's keyboard and pointer, and returns the screen's size.
pub(crate) fn input(directory: &Path, events: Vec<Input>) -> Result<ScreenSize> {
    let (_, response) = ask_for(directory, &Request::Input { events }, SCREEN_PATIENCE)?;
    response
        .screen
        .ok_or_else(|| not_answering(directory, "its answer says nothing of the screen"))
}

/// Where the machine's process passes VNC connections on to the guest's screen.
pub(crate) fn vnc_socket(directory: &Path) -> Result<PathBuf> {
    if !is_running(directory) {
        return Err(Error::new(ErrorCode::NotRunning, "The machine isn't running."));
    }
    // Asked, so that a process that is only just starting has written its note.
    status(directory)?;
    process_file(directory).and_then(|process| process.vnc).ok_or_else(|| {
        Error::new(
            ErrorCode::Protocol,
            format!(
                "The machine was started by an older build of {NAME}, which doesn't serve its screen. Stop it and start it again."
            ),
        )
    })
}

/// What the machine's process says about the machine. Whoever holds the machine's lock counts
/// as running it, and that may be a process that is still starting or someone who is deleting
/// the machine: only an answer tells.
pub(crate) fn status(directory: &Path) -> Result<Status> {
    let (_, status) = ask(directory, &Request::Status, PATIENCE)?;
    status.ok_or_else(|| not_answering(directory, "its answer says nothing"))
}

/// Turns the machine off and waits until its process has ended.
pub(crate) fn stop(directory: &Path, force: bool, timeout: Option<Duration>) -> Result<()> {
    let timeout = timeout.map(|timeout| timeout.min(LONGEST_STOP));
    let request = Request::Stop {
        force,
        timeout: timeout.map(|timeout| timeout.as_secs()),
    };
    let patience = timeout.unwrap_or(STOP_TIMEOUT) + STOP_MARGIN;
    match ask(directory, &request, patience) {
        Ok(_) => {}
        // It stopped by itself in the meantime, or ended before its answer got through.
        Err(_) if !is_running(directory) => return Ok(()),
        Err(error) => return Err(error),
    }
    // The machine is off. What comes next, a start or a deletion, needs its process gone too.
    let deadline = Instant::now() + PATIENCE;
    while is_running(directory) {
        if Instant::now() >= deadline {
            return Err(not_answering(
                directory,
                "the machine is off, but its process hasn't ended",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// A connection that carries the machine's console.
pub(crate) fn console(directory: &Path) -> Result<UnixStream> {
    ask(directory, &Request::Console, PATIENCE).map(|(stream, _)| stream)
}

/// A connection to `port` of the guest's socket device.
pub(crate) fn connect_guest(directory: &Path, port: u32) -> Result<UnixStream> {
    // The engine takes a few seconds to tell that nothing listens there.
    ask(directory, &Request::Connect { port }, 3 * PATIENCE).map(|(stream, _)| stream)
}
