//! What runs a machine: the system's virtualization, behind one trait.

#[cfg(target_os = "macos")]
mod mac;

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use crate::Result;

/// What a machine is made of, for an engine to run.
// Nothing reads it where there is no engine yet.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct Spec {
    pub cpus: u32,
    /// In bytes.
    pub memory: u64,
    /// The machine's disk, a raw image.
    pub disk: PathBuf,
    /// The disc that sets the guest up on its first start.
    pub seed: Option<PathBuf>,
    pub mac_address: String,
    /// The machine's folder, where an engine keeps what it has to remember about the machine.
    pub directory: PathBuf,
    /// The guest's end of its serial console.
    pub console: ConsoleEnd,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct ConsoleEnd {
    /// The guest reads what is typed to it from this.
    pub input: OwnedFd,
    /// The guest writes what it prints to this.
    pub output: OwnedFd,
}

/// Called when a machine stops without having been stopped through its engine: `None` when the
/// guest turned itself off, and otherwise what went wrong.
pub(crate) type Stopped = Box<dyn Fn(Option<String>) + Send + Sync>;

/// A machine that an engine has put together.
pub(crate) trait Engine: Send + Sync {
    fn start(&self) -> Result<()>;

    /// Asks the guest to shut down, as pressing a computer's power button does. Returns once it
    /// has been asked, not once it is off.
    fn request_stop(&self) -> Result<()>;

    /// Cuts the power. Returns once the machine is off, also when it was off already.
    fn stop(&self) -> Result<()>;

    /// A connection to `port` of the guest's socket device.
    fn connect(&self, port: u32) -> Result<OwnedFd>;
}

#[cfg(target_os = "macos")]
pub(crate) fn new(spec: Spec, stopped: Stopped) -> Result<Box<dyn Engine>> {
    Ok(Box::new(mac::Machine::new(spec, stopped)?))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn new(_spec: Spec, _stopped: Stopped) -> Result<Box<dyn Engine>> {
    Err(crate::Error::new(
        crate::ErrorCode::Unsupported,
        "Machines can't be started on this system yet.",
    ))
}
