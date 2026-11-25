//! The guest's agent, and what the host asks of it over a connection to its port in the guest.
//!
//! A connection carries one job. The host says hello with a [`Hello`] and the agent answers with
//! an [`AgentHello`]. The host then sends a [`Job`] as one line, and the agent answers with an
//! [`Answer`]. After a yes, what follows depends on the job:
//!
//! - [`Job::Info`], [`Job::SetClock`] and [`Job::Mount`]: nothing. The answer is all.
//! - [`Job::Freeze`]: nothing, until the host hangs up.
//! - [`Job::Exec`], [`Job::CopyIn`], [`Job::CopyOut`] and [`Job::Update`]: frames both ways,
//!   up to the agent's [`Kind::End`]. The host calls a job off by hanging up.
//! - [`Job::Tunnel`]: the bytes of a TCP connection in the guest, both ways.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Error, ErrorCode, Hello, receive, send};

/// The port in the guest that the agent listens on. It is below 1024, where only root may
/// listen, so that whoever answers there is the agent.
pub const AGENT_PORT: u32 = 717;
/// The newest version of the agent's protocol that this build speaks.
pub const AGENT_PROTOCOL: u32 = 1;

/// What the agent says about itself when a connection opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHello {
    pub protocol: u32,
    /// The agent's version.
    pub version: String,
    /// When the agent was built, in seconds since 1970: of two builds of one version, the later
    /// replaces the earlier. 0 when nobody said.
    #[serde(default)]
    pub built: u64,
    /// The guest's clock, in nanoseconds since 1970.
    #[serde(default)]
    pub time: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "kebab-case")]
pub enum Job {
    /// What the guest is and how it is doing: [`Answer::info`].
    Info,
    /// Sets the guest's clock to this time, in nanoseconds since 1970.
    SetClock {
        nanoseconds: u64,
    },
    Exec(Exec),
    /// Unpacks the tar stream that follows as input at `path`, as [`unpack`](crate::copy::unpack)
    /// does, as `user`.
    CopyIn {
        path: String,
        user: String,
    },
    /// Packs what is at `path` as a tar stream, as [`pack`](crate::copy::pack) does, as `user`,
    /// and sends it as output.
    CopyOut {
        path: String,
        user: String,
    },
    /// Connects to this TCP port of the guest itself.
    Tunnel {
        port: u16,
    },
    /// Replaces the agent with the program that follows as input, `size` bytes long. The agent
    /// ends once the connection is closed, and the system starts the new one.
    Update {
        size: u64,
    },
    /// Writes out what the guest's root file system holds and keeps it from changing, so that
    /// its disk can be taken as it is. The answer says when that is so. The file system is
    /// thawed once the host hangs up, or after `seconds` whatever happens.
    Freeze {
        seconds: u64,
    },
    /// Mounts the folders that the host shares, and unmounts the shared folders that aren't
    /// among them any more. The answer says when they are in place. The agent mounts the same
    /// again each time it starts, until it is told otherwise.
    Mount {
        mounts: Vec<Mount>,
    },
}

/// A virtio-fs device of the guest, mounted at a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// What the device is called.
    pub tag: String,
    /// Where it is mounted: [`SHARED`] or a folder in it.
    pub path: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// The device holds other folders than it did: it is mounted anew if it is mounted, which
    /// forgets what the guest kept of the folders before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fresh: bool,
}

/// The folder of the guest that shared folders are mounted in.
pub const SHARED: &str = "/mnt/shared";

/// A command to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exec {
    /// The program and its arguments. No shell reads them.
    pub arguments: Vec<String>,
    /// The account to run it as.
    pub user: String,
    /// Where to run it. A relative path is taken from the account's home folder, which is
    /// where it runs without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// Added to the command's environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    /// Seconds after which the command is ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// The agent's answer to a job: whether it takes it on. What goes wrong after a yes is said by
/// the frame that ends the job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<GuestInfo>,
}

impl Answer {
    pub fn yes() -> Self {
        Self {
            ok: true,
            error: None,
            info: None,
        }
    }

    pub fn no(error: Error) -> Self {
        Self {
            ok: false,
            error: Some(error),
            info: None,
        }
    }

    pub fn into_result(self) -> Result<Option<GuestInfo>, Error> {
        match (self.ok, self.error) {
            (true, _) => Ok(self.info),
            (false, Some(error)) => Err(error),
            (false, None) => Err(Error::new(
                ErrorCode::Guest,
                "The guest's agent refused, and didn't say why.",
            )),
        }
    }
}

/// What the guest is, as its agent sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestInfo {
    /// The agent's version.
    pub agent: String,
    pub host_name: String,
    /// The system's name and version, as it shows them: `Debian GNU/Linux 13 (trixie)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub kernel: String,
    pub arch: String,
    /// The addresses the guest holds, without those that only work on their own link.
    #[serde(default)]
    pub addresses: Vec<Address>,
    /// The system has finished starting, its first-start setup included.
    pub booted: bool,
    /// How the setup scripts are doing, in a guest that has any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Setup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub interface: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    pub state: SetupState,
    /// What is being set up, as the scripts call it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The step a script says it is at, or failed at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SetupState {
    /// The scripts run once the system's own first-start setup is over.
    Pending,
    Running,
    Finished,
    Failed,
    #[serde(other)]
    Unknown,
}

/// How a job ended: the payload of its last frame.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    /// The exit status of a command that came to its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<i32>,
    /// The signal that ended a command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// The command was ended because it ran out of time.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
    /// What went wrong, other than a command failing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

impl Ended {
    pub fn success(&self) -> bool {
        self.status == Some(0) && self.error.is_none()
    }
}

/// What a frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// From the host: bytes for the job's input. An empty one says that no more follow.
    Input = 0,
    /// From the agent: what the command printed.
    Output = 1,
    /// From the agent: what the command printed as errors.
    Errors = 2,
    /// From the agent, last: an [`Ended`] in JSON.
    End = 3,
}

/// No frame is sent with more than this in it.
pub const LONGEST_FRAME: usize = 64 * 1024;
/// A frame that says it is longer than this isn't one.
const LONGEST_ACCEPTED_FRAME: usize = 1024 * 1024;

/// Writes `bytes` as one frame: a byte for its kind, four for its length, most significant
/// first, and the bytes. In one write, so that frames from several threads don't mix.
pub fn write_frame(stream: &mut impl Write, kind: Kind, bytes: &[u8]) -> io::Result<()> {
    debug_assert!(bytes.len() <= LONGEST_ACCEPTED_FRAME);
    let mut frame = Vec::with_capacity(5 + bytes.len());
    frame.push(kind as u8);
    frame.extend((bytes.len() as u32).to_be_bytes());
    frame.extend(bytes);
    stream.write_all(&frame)
}

/// Reads the next frame. `None` when the other side hung up between two frames. Frames of a
/// kind that this build doesn't know are passed over.
pub fn read_frame(stream: &mut impl Read) -> io::Result<Option<(Kind, Vec<u8>)>> {
    loop {
        let mut header = [0; 5];
        let mut read = 0;
        while read < header.len() {
            match stream.read(&mut header[read..]) {
                Ok(0) if read == 0 => return Ok(None),
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) => read += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
        if length > LONGEST_ACCEPTED_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a frame is too long"));
        }
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload)?;
        let kind = match header[0] {
            0 => Kind::Input,
            1 => Kind::Output,
            2 => Kind::Errors,
            3 => Kind::End,
            _ => continue,
        };
        return Ok(Some((kind, payload)));
    }
}

/// Writes what `source` reads as frames of `kind`, and then an empty one.
pub fn write_all_frames(stream: &mut impl Write, kind: Kind, source: &mut impl Read) -> io::Result<()> {
    let mut buffer = vec![0; LONGEST_FRAME];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => return write_frame(stream, kind, &[]),
            Ok(count) => write_frame(stream, kind, &buffer[..count])?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn lost(error: io::Error) -> Error {
    Error::new(
        ErrorCode::Guest,
        format!("The connection to the guest's agent failed: {error}."),
    )
}

/// The host's part of the hello. Returns what the agent says about itself.
pub fn greet_agent(stream: &mut UnixStream, program_version: &str) -> Result<AgentHello, Error> {
    let hello = Hello {
        protocol: AGENT_PROTOCOL,
        version: program_version.to_string(),
    };
    send(stream, &hello).map_err(lost)?;
    let theirs: AgentHello = receive(stream)
        .map_err(lost)?
        .ok_or_else(|| lost(io::ErrorKind::UnexpectedEof.into()))?;
    if theirs.protocol < 1 {
        return Err(Error::new(
            ErrorCode::Protocol,
            "The guest's agent speaks a protocol that this program doesn't.",
        ));
    }
    Ok(theirs)
}

/// The agent's part of the hello.
pub fn welcome_host(stream: &mut UnixStream, hello: &AgentHello) -> Result<u32, Error> {
    let theirs: Hello = receive(stream)
        .map_err(lost)?
        .ok_or_else(|| lost(io::ErrorKind::UnexpectedEof.into()))?;
    send(stream, hello).map_err(lost)?;
    Ok(theirs.protocol.min(AGENT_PROTOCOL))
}

/// Sends `job` and returns the agent's answer: what it says on a yes, or why it said no.
pub fn ask(stream: &mut UnixStream, job: &Job) -> Result<Option<GuestInfo>, Error> {
    send(stream, job).map_err(lost)?;
    receive::<Answer>(stream)
        .map_err(lost)?
        .ok_or_else(|| lost(io::ErrorKind::UnexpectedEof.into()))?
        .into_result()
}

/// The output and the end of a job that the agent has said yes to. `input` is sent as the
/// job's input, from a thread of its own; without one, the input is empty.
///
/// A thread that is still reading `input` when the job ends is left to itself.
pub fn run_job(
    stream: &UnixStream,
    input: Option<Box<dyn Read + Send>>,
    output: &mut dyn Write,
    errors: &mut dyn Write,
) -> Result<Ended, Error> {
    let mut sending = stream.try_clone().map_err(lost)?;
    match input {
        // A failure to send shows as the job ending without its end.
        Some(mut input) => {
            std::thread::spawn(move || _ = write_all_frames(&mut sending, Kind::Input, &mut input));
        }
        None => write_frame(&mut sending, Kind::Input, &[]).map_err(lost)?,
    }
    run_frames(stream, output, errors)
}

fn written(error: io::Error) -> Error {
    Error::new(
        ErrorCode::Io,
        format!("Couldn't pass the guest's output on: {error}."),
    )
}

/// What a copy job that failed said about it.
fn copy_failed(ended: Ended, errors: &[u8]) -> Error {
    if let Some(error) = ended.error {
        return error;
    }
    let said = String::from_utf8_lossy(errors).trim().to_string();
    let message = if said.is_empty() {
        "The copy failed in the guest, which didn't say why.".to_string()
    } else {
        said
    };
    Error::new(ErrorCode::Guest, message)
}

/// Sends what is at `source` on this computer for a [`Job::CopyIn`] that the agent has said yes
/// to, and returns once the guest has it.
pub fn send_copy(stream: &UnixStream, source: &Path) -> Result<(), Error> {
    let mut sending = stream.try_clone().map_err(lost)?;
    let source = source.to_owned();
    let packing = std::thread::spawn(move || {
        let mut frames = FrameWriter(&mut sending);
        let packed = crate::copy::pack(&source, &mut frames).and_then(|()| frames.0.flush());
        match packed {
            Ok(()) => write_frame(frames.0, Kind::Input, &[]).map_err(lost),
            Err(error) => {
                // The guest is left with half a copy, which the hang-up calls off.
                _ = frames.0.shutdown(std::net::Shutdown::Both);
                Err(Error::new(
                    ErrorCode::Io,
                    format!("Couldn't read {}: {error}.", source.display()),
                ))
            }
        }
    });
    let mut errors = Vec::new();
    let ended = run_frames(stream, &mut std::io::sink(), &mut errors);
    let packed = packing.join().expect("packing doesn't panic");
    match ended {
        Ok(ended) if ended.success() => packed,
        // The guest's reason first: a guest that gave up ends the packing with a broken pipe.
        Ok(ended) => Err(copy_failed(ended, &errors)),
        // Packing that failed here hangs up, which is all the guest hears of it.
        Err(lost) => packed.and(Err(lost)),
    }
}

/// Receives what a [`Job::CopyOut`] of `source` that the agent has said yes to sends, and puts
/// it at `destination` on this computer. Nothing but what `source` names is written: the guest
/// isn't trusted. A source that names no file or folder of its own, such as `.`, can only be
/// copied to a destination that isn't a folder.
pub fn receive_copy(stream: &UnixStream, source: &str, destination: &Path) -> Result<(), Error> {
    let expected = Path::new(source).file_name();
    if expected.is_none() && destination.is_dir() {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            format!(
                "\"{source}\" has no name to copy it into {} under. Give the copy's own path.",
                destination.display()
            ),
        ));
    }
    let mut sending = stream;
    write_frame(&mut sending, Kind::Input, &[]).map_err(lost)?;
    let mut reader = FrameReader {
        stream,
        pending: Vec::new(),
        offset: 0,
        errors: Vec::new(),
        ended: None,
    };
    let unpacked = crate::copy::unpack(&mut reader, destination, expected);
    // What follows the archive's end, the end of the job among it.
    if reader.ended.is_none() {
        _ = std::io::copy(&mut reader, &mut std::io::sink());
    }
    _ = stream.shutdown(std::net::Shutdown::Both);
    let Some(ended) = reader.ended.take() else {
        return match unpacked {
            Err(error) if !reader.errors.is_empty() || error.kind() != io::ErrorKind::UnexpectedEof => {
                Err(Error::new(
                    ErrorCode::Io,
                    format!("Couldn't copy to {}: {error}.", destination.display()),
                ))
            }
            _ => Err(lost(io::ErrorKind::UnexpectedEof.into())),
        };
    };
    if !ended.success() {
        return Err(copy_failed(ended, &reader.errors));
    }
    unpacked.map_err(|error| {
        Error::new(
            ErrorCode::Io,
            format!("Couldn't copy to {}: {error}.", destination.display()),
        )
    })
}

/// Reads frames until the end of the job, passing output and errors on.
fn run_frames(stream: &UnixStream, output: &mut dyn Write, errors: &mut dyn Write) -> Result<Ended, Error> {
    let mut receiving = stream;
    loop {
        match read_frame(&mut receiving).map_err(lost)? {
            Some((Kind::Output, bytes)) => output.write_all(&bytes).map_err(written)?,
            Some((Kind::Errors, bytes)) => errors.write_all(&bytes).map_err(written)?,
            Some((Kind::End, bytes)) => {
                // Lets the agent go, which waits for this side to hang up.
                _ = stream.shutdown(std::net::Shutdown::Both);
                return serde_json::from_slice(&bytes).map_err(|_| {
                    Error::new(
                        ErrorCode::Protocol,
                        "The guest's agent ended the job without saying how.",
                    )
                });
            }
            Some((Kind::Input, _)) => {}
            None => return Err(lost(io::ErrorKind::UnexpectedEof.into())),
        }
    }
}

/// Writes input frames of no more than [`LONGEST_FRAME`], collected from small writes.
struct FrameWriter<'a>(&'a mut UnixStream);

impl Write for FrameWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(LONGEST_FRAME);
        if count > 0 {
            write_frame(self.0, Kind::Input, &bytes[..count])?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The output frames of a job as one stream of bytes. Error frames are kept, and the end frame
/// ends the stream.
struct FrameReader<'a> {
    stream: &'a UnixStream,
    pending: Vec<u8>,
    offset: usize,
    errors: Vec<u8>,
    ended: Option<Ended>,
}

/// What is kept of what a job prints as errors.
const KEPT_ERRORS: usize = 64 * 1024;

impl Read for FrameReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        while self.offset == self.pending.len() {
            if self.ended.is_some() {
                return Ok(0);
            }
            let mut stream = self.stream;
            match read_frame(&mut stream)? {
                Some((Kind::Output, bytes)) => {
                    self.pending = bytes;
                    self.offset = 0;
                }
                Some((Kind::Errors, bytes)) => {
                    let room = KEPT_ERRORS.saturating_sub(self.errors.len());
                    self.errors.extend(&bytes[..bytes.len().min(room)]);
                }
                Some((Kind::End, bytes)) => {
                    self.ended = Some(serde_json::from_slice(&bytes).unwrap_or_else(|_| Ended {
                        error: Some(Error::new(
                            ErrorCode::Protocol,
                            "The guest's agent ended the job without saying how.",
                        )),
                        ..Ended::default()
                    }));
                }
                Some((Kind::Input, _)) => {}
                None => return Err(io::ErrorKind::UnexpectedEof.into()),
            }
        }
        let count = buffer.len().min(self.pending.len() - self.offset);
        buffer[..count].copy_from_slice(&self.pending[self.offset..][..count]);
        self.offset += count;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What is on the line is what other programs are written against.
    #[test]
    fn jobs_keep_their_spelling() {
        let exec = Job::Exec(Exec {
            arguments: vec!["uname".into(), "-a".into()],
            user: "username".into(),
            directory: None,
            environment: BTreeMap::new(),
            timeout: Some(5),
        });
        assert_eq!(
            serde_json::to_string(&exec).unwrap(),
            r#"{"job":"exec","arguments":["uname","-a"],"user":"username","timeout":5}"#
        );
        assert_eq!(
            serde_json::to_string(&Job::Tunnel { port: 22 }).unwrap(),
            r#"{"job":"tunnel","port":22}"#
        );
        let mount = Job::Mount {
            mounts: vec![
                Mount {
                    tag: "src".into(),
                    path: "/mnt/shared/src".into(),
                    read_only: true,
                    fresh: false,
                },
                Mount {
                    tag: "svm".into(),
                    path: SHARED.into(),
                    read_only: false,
                    fresh: true,
                },
            ],
        };
        assert_eq!(
            serde_json::to_string(&mount).unwrap(),
            r#"{"job":"mount","mounts":[{"tag":"src","path":"/mnt/shared/src","read_only":true},{"tag":"svm","path":"/mnt/shared","fresh":true}]}"#
        );
        let ended = Ended {
            signal: Some(15),
            timed_out: true,
            ..Ended::default()
        };
        assert_eq!(
            serde_json::to_string(&ended).unwrap(),
            r#"{"signal":15,"timed_out":true}"#
        );
        let setup: Setup = serde_json::from_str(r#"{"state":"brand-new"}"#).unwrap();
        assert_eq!(setup.state, SetupState::Unknown);
    }

    #[test]
    fn frames_are_read_back_and_unknown_ones_passed_over() {
        let mut line = Vec::new();
        write_frame(&mut line, Kind::Output, b"hello").unwrap();
        line.extend([9, 0, 0, 0, 2, b'?', b'?']);
        write_frame(&mut line, Kind::End, b"{}").unwrap();
        let mut reading = line.as_slice();
        assert_eq!(
            read_frame(&mut reading).unwrap(),
            Some((Kind::Output, b"hello".to_vec()))
        );
        assert_eq!(
            read_frame(&mut reading).unwrap(),
            Some((Kind::End, b"{}".to_vec()))
        );
        assert_eq!(read_frame(&mut reading).unwrap(), None);

        let mut torn: &[u8] = &[1, 0, 0];
        assert!(read_frame(&mut torn).is_err());
    }
}
