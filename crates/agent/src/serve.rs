//! One connection from the host: a hello, a job, and what the job carries.

use std::fs::File;
use std::io::Write;
use std::net::TcpStream;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use proto::agent::{AGENT_PROTOCOL, AgentHello, Answer, Ended, Job, Kind, read_frame, write_frame};
use proto::{Error, ErrorCode};

use crate::{job, system};

/// How long the host gets to say hello and what it wants.
const ASKING: Duration = Duration::from_secs(30);

pub struct Agent {
    /// This program, which `cp` runs to pack and unpack, and which an update replaces.
    program: PathBuf,
}

/// When this program was built, which the host's build says when it builds it.
const BUILT: Option<&str> = option_env!("AGENT_BUILT");

/// Where this program is. Linux names a program that was replaced while it ran with a note
/// after its path, and the new one is where the old one was.
fn own_program() -> PathBuf {
    let path = std::env::current_exe().unwrap_or_default();
    let text = path.to_string_lossy();
    match text.strip_suffix(" (deleted)") {
        Some(path) => PathBuf::from(path),
        None => path,
    }
}

fn answer(stream: &mut UnixStream, answer: &Answer) {
    _ = proto::send(stream, answer);
}

impl Agent {
    pub fn new() -> Self {
        Self {
            program: own_program(),
        }
    }

    fn hello(&self) -> AgentHello {
        AgentHello {
            protocol: AGENT_PROTOCOL,
            version: env!("CARGO_PKG_VERSION").to_string(),
            built: BUILT.and_then(|built| built.parse().ok()).unwrap_or(0),
            time: system::now(),
        }
    }

    /// Serves one connection.
    pub fn handle(&self, mut stream: UnixStream) {
        // Someone who connects and says nothing is let go of.
        _ = stream.set_read_timeout(Some(ASKING));
        if proto::agent::welcome_host(&mut stream, &self.hello()).is_err() {
            return;
        }
        let job = match proto::receive::<Job>(&mut stream) {
            Ok(Some(job)) => job,
            // Most likely a job of a newer version.
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                let unknown = Error::new(ErrorCode::Protocol, "The guest's agent doesn't know this job.");
                return answer(&mut stream, &Answer::no(unknown));
            }
            Ok(None) | Err(_) => return,
        };
        _ = stream.set_read_timeout(None);
        match job {
            Job::Info => answer(
                &mut stream,
                &Answer {
                    info: Some(system::info()),
                    ..Answer::yes()
                },
            ),
            Job::SetClock { nanoseconds } => answer(
                &mut stream,
                &match system::set_clock(nanoseconds) {
                    Ok(()) => Answer::yes(),
                    Err(error) => Answer::no(error),
                },
            ),
            Job::Exec(exec) => job::run(stream, &exec),
            Job::CopyIn { path, user } => job::run_self(stream, &self.program, &["unpack", &path], user),
            Job::CopyOut { path, user } => job::run_self(stream, &self.program, &["pack", &path], user),
            Job::Tunnel { port } => tunnel(stream, port),
            Job::Update { size } => self.update(stream, size),
        }
    }

    /// Replaces this program with the one that arrives, and ends once the host has hung up. The
    /// system starts the new one. Commands that run are left running, but connections to them
    /// end with this program.
    fn update(&self, mut stream: UnixStream, size: u64) {
        let mut name = self.program.clone().into_os_string();
        name.push(".new");
        let new = PathBuf::from(name);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&new);
        let mut file = match file {
            Ok(file) => file,
            Err(error) => {
                let error = Error::new(
                    ErrorCode::Io,
                    format!("Couldn't write {}: {error}.", new.display()),
                );
                return answer(&mut stream, &Answer::no(error));
            }
        };
        answer(&mut stream, &Answer::yes());
        let received = receive_program(&mut stream, &mut file, size)
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::rename(&new, &self.program));
        let ended = match &received {
            Ok(()) => Ended {
                status: Some(0),
                ..Ended::default()
            },
            Err(error) => {
                _ = std::fs::remove_file(&new);
                Ended {
                    error: Some(Error::new(
                        ErrorCode::Io,
                        format!("Couldn't replace the agent: {error}."),
                    )),
                    ..Ended::default()
                }
            }
        };
        let end = serde_json::to_vec(&ended).expect("plain data");
        if write_frame(&mut stream, Kind::End, &end).is_ok() {
            _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            _ = read_frame(&mut stream);
        }
        if received.is_ok() {
            eprintln!("Replaced by a new build. The system starts it.");
            std::process::exit(0);
        }
    }
}

fn receive_program(stream: &mut UnixStream, file: &mut File, size: u64) -> std::io::Result<()> {
    let mut received = 0;
    loop {
        match read_frame(stream)? {
            Some((Kind::Input, bytes)) if bytes.is_empty() => break,
            Some((Kind::Input, bytes)) => {
                received += bytes.len() as u64;
                if received > size {
                    break;
                }
                file.write_all(&bytes)?;
            }
            Some(_) => {}
            None => return Err(std::io::ErrorKind::UnexpectedEof.into()),
        }
    }
    if received != size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{received} bytes arrived of {size}"),
        ));
    }
    Ok(())
}

/// Connects to `port` of the guest itself and relays the connection.
fn tunnel(mut stream: UnixStream, port: u16) {
    let connected = TcpStream::connect(("127.0.0.1", port)).or_else(|_| TcpStream::connect(("::1", port)));
    match connected {
        Ok(connection) => {
            answer(&mut stream, &Answer::yes());
            proto::relay::relay(stream, connection);
        }
        Err(error) => {
            let error = Error::new(
                ErrorCode::Guest,
                format!("Nothing in the guest answers on port {port}: {error}."),
            );
            answer(&mut stream, &Answer::no(error));
        }
    }
}
