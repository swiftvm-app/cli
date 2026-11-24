use std::fmt;

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

/// What went wrong, for a program to act on. A code keeps its spelling and its meaning once it
/// has been released; new ones may be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// A name, a size or an option that can't be used.
    InvalidArgument,
    /// No machine or image goes by that name.
    NotFound,
    AlreadyExists,
    /// The machine is running, and has to be stopped for this.
    Running,
    /// The machine isn't running, and has to be for this.
    NotRunning,
    /// An image couldn't be downloaded.
    Download,
    /// A download isn't what its publisher says it should be.
    Checksum,
    /// An image is damaged, or in a format that isn't read.
    Image,
    /// This computer or this build can't do it.
    Unsupported,
    /// The virtualization engine refused or failed.
    Engine,
    /// The other side of a connection said something that wasn't understood.
    Protocol,
    /// A file or a socket failed.
    Io,
    /// The guest's agent doesn't answer, or nothing in the guest listens on a port.
    Guest,
    /// Something took longer than it was given.
    TimedOut,
    /// An image that machines are built on, which can't go before they do.
    InUse,
    /// A code from a newer version.
    #[serde(other)]
    Unknown,
}

/// An error as it is shown to a person (`message`) and to a program (`code`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
