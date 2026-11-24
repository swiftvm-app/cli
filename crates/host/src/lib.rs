//! Creates and runs virtual machines.
//!
//! No daemon keeps the machines. They are folders in a store ([`Store`]), which any number of
//! programs may read and change at the same time, and each running machine has a process of its
//! own ([`process`]) that is reached over a socket. [`Host`] puts the two together.

mod client;
mod console;
mod desktop;
mod download;
mod engine;
mod guest;
mod host;
mod images;
mod import;
mod iso;
mod keys;
mod layers;
mod lock;
mod paths;
mod png;
pub mod process;
mod qcow2;
mod rfb;
mod saved;
mod screen;
mod scripts;
mod seed;
mod store;

pub use desktop::{Button, Direction, Picture};
pub use disk::layer::CLUSTER as LAYER_CLUSTER;
pub use host::{
    CreateOptions, DiskCheck, DiskInfo, ExecOptions, Host, ImageSummary, LayerCheck, LayerInfo, Machine,
    MachineState, Progress, SnapshotInfo,
};
pub use paths::{cache_directory, home_directory};
pub use proto::agent::{Address, Ended, GuestInfo, Setup, SetupState};
pub use proto::relay::relay;
pub use proto::{Error, ErrorCode, Input, NAME, Result, SCREEN_PORT, ScreenSize};
pub use saved::ImageKind;
pub use scripts::built_in as setup_scripts;
pub use store::{Automatic, Config, DiskConfig, DiskFormat, Snapshot, Store};

use std::fmt::Display;
use std::path::Path;

/// Turns a failure of the system into an error that says what was being done.
pub(crate) trait Context<T> {
    /// `doing` completes "Couldn't …": `"read the machine's settings"`.
    fn context(self, doing: impl Display) -> Result<T>;

    /// For a failure that is about one file.
    fn at(self, doing: &str, path: &Path) -> Result<T>;
}

impl<T> Context<T> for std::io::Result<T> {
    fn context(self, doing: impl Display) -> Result<T> {
        self.map_err(|error| Error::new(ErrorCode::Io, format!("Couldn't {doing}: {error}.")))
    }

    fn at(self, doing: &str, path: &Path) -> Result<T> {
        self.context(format_args!("{doing} {}", path.display()))
    }
}

/// Seconds since 1970.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Bytes that nobody can guess, as hexadecimal digits.
pub(crate) fn random_hex(bytes: usize) -> Result<String> {
    use std::io::Read;
    let mut buffer = vec![0; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut buffer))
        .context("read random numbers from the system")?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The guest's agent, a static Linux program for this computer's processor.
pub(crate) const AGENT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/agent"));
/// When it was built, in seconds since 1970.
pub(crate) const AGENT_BUILT: &str = env!("AGENT_BUILT");
