//! The control socket of a machine's process: one JSON document per line.
//!
//! Each side first sends a [`Hello`]. The client then sends [`Request`]s and reads a [`Response`]
//! to each. After the answer to [`Request::Console`] the connection carries the console's bytes
//! both ways instead.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Error, ErrorCode};

/// The newest version of the protocol that this build speaks.
pub const PROTOCOL: u32 = 1;
/// The oldest one. A machine keeps the process it was started with, so a newer command has to
/// talk to an older process, and the other way round after a downgrade.
pub const OLDEST_PROTOCOL: u32 = 1;

/// A line that is longer than this isn't a message.
pub const LONGEST_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The newest version of the protocol that the sender speaks.
    pub protocol: u32,
    /// The version of the sender's program, for messages to people.
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "kebab-case")]
pub enum Request {
    Status,
    /// Turns the machine off, and answers once it is off.
    Stop {
        /// Cut the power, without asking the guest to shut down first.
        #[serde(default)]
        force: bool,
        /// How many seconds the guest gets to shut down before its power is cut.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Console,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Starting,
    Running,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub state: State,
    /// The machine's process.
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

impl Response {
    pub fn done(status: Option<Status>) -> Self {
        Self {
            ok: true,
            error: None,
            status,
        }
    }

    pub fn failed(error: Error) -> Self {
        Self {
            ok: false,
            error: Some(error),
            status: None,
        }
    }

    pub fn into_result(self) -> Result<Option<Status>, Error> {
        match (self.ok, self.error) {
            (true, _) => Ok(self.status),
            (false, Some(error)) => Err(error),
            (false, None) => Err(Error::new(
                ErrorCode::Protocol,
                "The request failed, and nothing said why.",
            )),
        }
    }
}

/// Writes a message as one line.
pub fn send<T: Serialize>(stream: &mut impl Write, message: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    stream.write_all(&line)
}

/// Reads the next line as a message. `None` when the other side hung up between two messages.
///
/// A byte at a time, so that nothing after the line is taken: what follows the answer to
/// [`Request::Console`] isn't a message.
pub fn receive<T: DeserializeOwned>(stream: &mut impl Read) -> io::Result<Option<T>> {
    let mut line = Vec::new();
    let mut byte = [0];
    loop {
        match stream.read(&mut byte) {
            Ok(0) if line.is_empty() => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if line.len() == LONGEST_LINE => return Err(io::ErrorKind::InvalidData.into()),
            Ok(_) => line.push(byte[0]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    // A line that isn't a message is told apart from a connection that failed: the next line
    // may be one again.
    match serde_json::from_slice(&line) {
        Ok(message) => Ok(Some(message)),
        Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error)),
    }
}

/// The version that two sides speak to each other: the newest that both know.
pub fn agreed(theirs: u32) -> Result<u32, Error> {
    let version = theirs.min(PROTOCOL);
    if version < OLDEST_PROTOCOL {
        return Err(Error::new(
            ErrorCode::Protocol,
            format!(
                "The other side speaks version {theirs} of the protocol, which is too old for this program."
            ),
        ));
    }
    Ok(version)
}

fn hello(program_version: &str) -> Hello {
    Hello {
        protocol: PROTOCOL,
        version: program_version.to_string(),
    }
}

fn unreadable(error: io::Error) -> Error {
    Error::new(
        ErrorCode::Protocol,
        format!("The connection failed before both sides had said hello: {error}."),
    )
}

/// The client's part of the handshake. Returns the version to speak.
pub fn greet<S: Read + Write>(stream: &mut S, program_version: &str) -> Result<u32, Error> {
    send(stream, &hello(program_version)).map_err(unreadable)?;
    match receive::<Hello>(stream).map_err(unreadable)? {
        Some(theirs) => agreed(theirs.protocol),
        None => Err(unreadable(io::ErrorKind::UnexpectedEof.into())),
    }
}

/// The server's part of the handshake. Returns the version to speak.
pub fn welcome<S: Read + Write>(stream: &mut S, program_version: &str) -> Result<u32, Error> {
    let theirs = receive::<Hello>(stream).map_err(unreadable)?;
    send(stream, &hello(program_version)).map_err(unreadable)?;
    match theirs {
        Some(theirs) => agreed(theirs.protocol),
        None => Err(unreadable(io::ErrorKind::UnexpectedEof.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What is on the line is what other programs are written against.
    #[test]
    fn messages_keep_their_spelling() {
        let stop = Request::Stop {
            force: true,
            timeout: Some(5),
        };
        assert_eq!(
            serde_json::to_string(&stop).unwrap(),
            r#"{"request":"stop","force":true,"timeout":5}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"request":"stop"}"#).unwrap(),
            Request::Stop {
                force: false,
                timeout: None
            }
        );

        let refused = Response::failed(Error::new(ErrorCode::NotRunning, "No."));
        assert_eq!(
            serde_json::to_string(&refused).unwrap(),
            r#"{"ok":false,"error":{"code":"not-running","message":"No."}}"#
        );
        let running = Response::done(Some(Status {
            state: State::Running,
            pid: 7,
        }));
        assert_eq!(
            serde_json::to_string(&running).unwrap(),
            r#"{"ok":true,"status":{"state":"running","pid":7}}"#
        );
    }

    #[test]
    fn what_a_newer_version_adds_is_read_past() {
        let newer = r#"{"ok":false,"error":{"code":"brand-new","message":"No.","hint":1},"extra":[]}"#;
        let response: Response = serde_json::from_str(newer).unwrap();
        assert_eq!(response.error.unwrap().code, ErrorCode::Unknown);

        let mut lines: &[u8] = b"{\"request\":\"teleport\"}\n{\"request\":\"status\"}\n";
        assert_eq!(
            receive::<Request>(&mut lines).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(receive::<Request>(&mut lines).unwrap(), Some(Request::Status));
        assert_eq!(receive::<Request>(&mut lines).unwrap(), None);
    }
}
