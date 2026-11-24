//! Runs a command for the host: its input, output and errors travel as frames, and its exit
//! status ends the job.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use proto::agent::{Answer, Ended, Exec, Kind, LONGEST_FRAME, read_frame, write_frame};
use proto::{Error, ErrorCode};

use crate::account;

/// Where commands look for programs: what a login gives root on the systems that are run.
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// How long a command that was told to end gets before it is killed.
const KILL_GRACE: Duration = Duration::from_secs(3);
/// What a command left running in the background may hold its output open for good. Once the
/// command has ended, that output gets this long to come to its end.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
/// How often a running command's job looks up to see whether it is called off.
const LOOK_UP: Duration = Duration::from_millis(100);
/// How long the host gets to hang up once the job has ended.
const HANG_UP_GRACE: Duration = Duration::from_secs(5);

/// The command that `exec` asks for, ready to start.
fn command(exec: &Exec) -> Result<Command, Error> {
    let Some((program, arguments)) = exec.arguments.split_first() else {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            "Say which command to run.",
        ));
    };
    let account = account::find(&exec.user).ok_or_else(|| {
        Error::new(
            ErrorCode::NotFound,
            format!("There is no account \"{}\" in the guest.", exec.user),
        )
    })?;
    let directory = match &exec.directory {
        // An absolute path replaces the home folder.
        Some(directory) => account.home.join(directory),
        None if account.home.is_dir() => account.home.clone(),
        None => "/".into(),
    };
    if !directory.is_dir() {
        return Err(Error::new(
            ErrorCode::NotFound,
            format!("There is no folder {} in the guest.", directory.display()),
        ));
    }
    let mut command = Command::new(program);
    command
        .args(arguments)
        .env_clear()
        .env("HOME", &account.home)
        .env("USER", &account.name)
        .env("LOGNAME", &account.name)
        .env("SHELL", &account.shell)
        .env("PATH", PATH)
        .envs(&exec.environment)
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A group of its own, which is ended as a whole: the command and what it started.
        .process_group(0);
    // Safety: geteuid can't fail.
    if account.uid != unsafe { libc::geteuid() } {
        let switch = account.switch().map_err(|error| {
            Error::new(
                ErrorCode::Io,
                format!("Couldn't switch to {}: {error}.", account.name),
            )
        })?;
        // Safety: the closure makes system calls only.
        unsafe { command.pre_exec(switch) };
    }
    Ok(command)
}

/// Ends the command's process group: politely, and after a while not.
fn end(group: i32) {
    // Safety: signals to a process group that this agent started.
    unsafe { libc::kill(-group, libc::SIGTERM) };
    std::thread::spawn(move || {
        std::thread::sleep(KILL_GRACE);
        // Safety: as above.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    });
}

type Sender = Arc<Mutex<Option<UnixStream>>>;

/// Sends what `from` reads as frames of `kind`, until it ends or the job does.
fn pass_on(mut from: impl Read, kind: Kind, to: Sender, done: mpsc::Sender<()>) {
    let mut buffer = vec![0; LONGEST_FRAME];
    loop {
        let count = match from.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let mut to = to.lock().unwrap();
        let Some(stream) = to.as_mut() else { break };
        if write_frame(stream, kind, &buffer[..count]).is_err() {
            *to = None;
            break;
        }
    }
    _ = done.send(());
}

/// Passes the host's input frames on to the command, and says on `hung_up` when the host hangs
/// up, which calls the job off.
fn feed(mut from: UnixStream, mut to: Option<impl Write>, hung_up: mpsc::Sender<()>) {
    loop {
        match read_frame(&mut from) {
            Ok(Some((Kind::Input, bytes))) if bytes.is_empty() => to = None,
            Ok(Some((Kind::Input, bytes))) => {
                // A command that has stopped reading takes no more.
                if let Some(input) = &mut to
                    && input.write_all(&bytes).is_err()
                {
                    to = None;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                _ = hung_up.send(());
                return;
            }
        }
    }
}

/// Runs `exec` for the host on `stream`, from the answer to the end of the job.
pub fn run(mut stream: UnixStream, exec: &Exec) {
    let started = command(exec).and_then(|mut command| {
        command.spawn().map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Error::new(
                ErrorCode::NotFound,
                format!("There is no program \"{}\" in the guest.", exec.arguments[0]),
            ),
            _ => Error::new(
                ErrorCode::Io,
                format!("Couldn't start \"{}\": {error}.", exec.arguments[0]),
            ),
        })
    });
    let mut child = match started {
        Ok(child) => child,
        Err(error) => {
            _ = proto::send(&mut stream, &Answer::no(error));
            return;
        }
    };
    let group = child.id() as i32;
    if proto::send(&mut stream, &Answer::yes()).is_err() {
        end(group);
        _ = child.wait();
        return;
    }
    let ended = watch(&mut child, &stream, exec.timeout.map(Duration::from_secs));
    if let Some((ended, hung_up)) = ended {
        finish(&mut stream, &ended, &hung_up);
    }
}

/// Passes the command's input and output on until it ends. `None` when the host hung up first,
/// and there is nobody to tell how it ended.
fn watch(
    child: &mut Child,
    stream: &UnixStream,
    timeout: Option<Duration>,
) -> Option<(Ended, mpsc::Receiver<()>)> {
    let group = child.id() as i32;
    let sender: Sender = Arc::new(Mutex::new(stream.try_clone().ok()));
    let (output_done, outputs) = mpsc::channel();
    for (output, kind) in [
        (
            child
                .stdout
                .take()
                .map(|output| Box::new(output) as Box<dyn Read + Send>),
            Kind::Output,
        ),
        (
            child
                .stderr
                .take()
                .map(|output| Box::new(output) as Box<dyn Read + Send>),
            Kind::Errors,
        ),
    ] {
        let (sender, done) = (sender.clone(), output_done.clone());
        let output = output.expect("the output is a pipe");
        std::thread::spawn(move || pass_on(output, kind, sender, done));
    }
    let (hang_up, hung_up) = mpsc::channel();
    {
        let input = child.stdin.take();
        let from = stream.try_clone().ok()?;
        std::thread::spawn(move || feed(from, input, hang_up));
    }
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let mut timed_out = false;
    let (waited, exits) = mpsc::channel();
    // The child is waited for on a thread of its own, which takes its id rather than the child.
    std::thread::spawn(move || {
        let mut status = 0;
        // Safety: waits for the agent's own child.
        let result = loop {
            match unsafe { libc::waitpid(group, &mut status, 0) } {
                -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
                -1 => break Err(std::io::Error::last_os_error()),
                _ => break Ok(ExitStatus::from_raw(status)),
            }
        };
        _ = waited.send(result);
    });
    // Looks up now and then for the host hanging up, and for the time running out.
    let status = loop {
        let wait = match deadline {
            Some(deadline) if !timed_out => deadline.saturating_duration_since(Instant::now()).min(LOOK_UP),
            _ => LOOK_UP,
        };
        if let Ok(status) = exits.recv_timeout(wait) {
            break status;
        }
        if hung_up.try_recv().is_ok() {
            end(group);
            return None;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) && !timed_out {
            timed_out = true;
            end(group);
        }
    };
    // What is left running in the background may hold the output open: once the command has
    // ended, it gets a moment, and what it says after that isn't sent.
    let deadline = Instant::now() + OUTPUT_GRACE;
    for _ in 0..2 {
        if outputs
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_err()
        {
            break;
        }
    }
    sender.lock().unwrap().take();
    let ended = match status {
        Ok(status) => Ended {
            status: status.code(),
            signal: status.signal(),
            timed_out,
            error: None,
        },
        Err(error) => Ended {
            error: Some(Error::new(
                ErrorCode::Io,
                format!("Couldn't wait for the command: {error}."),
            )),
            ..Ended::default()
        },
    };
    Some((ended, hung_up))
}

/// Sends the frame that ends the job, and waits for the host to hang up: one that hangs up from
/// this end while input is still arriving could lose that frame.
fn finish(stream: &mut UnixStream, ended: &Ended, hung_up: &mpsc::Receiver<()>) {
    let end = serde_json::to_vec(ended).expect("plain data");
    if write_frame(stream, Kind::End, &end).is_ok() {
        _ = hung_up.recv_timeout(HANG_UP_GRACE);
    }
}

/// Runs this agent's own program with `arguments`, as `exec` would run a command.
pub fn run_self(stream: UnixStream, program: &Path, arguments: &[&str], user: String) {
    let mut all = vec![program.to_string_lossy().into_owned()];
    all.extend(arguments.iter().map(|argument| argument.to_string()));
    let exec = Exec {
        arguments: all,
        user,
        directory: None,
        environment: Default::default(),
        timeout: None,
    };
    run(stream, &exec);
}
