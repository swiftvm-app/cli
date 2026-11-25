//! What runs a machine: the system's virtualization, behind one trait.
//!
//! On a Mac that is Apple's Virtualization framework, in this process. On Linux it is KVM, through
//! a cloud-hypervisor process and a passt process that this one starts ([`tools`] finds them).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod mac;
pub(crate) mod tools;

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use proto::agent::{Mount, SHARED};

use crate::store::Share;
use crate::{Error, ErrorCode, Result};

/// What a machine is made of, for an engine to run.
pub(crate) struct Spec {
    /// The machine's, which names the sockets that don't fit in its folder.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub id: String,
    pub cpus: u32,
    /// In bytes.
    pub memory: u64,
    pub disk: DiskAttachment,
    /// The disc that sets the guest up on its first start.
    pub seed: Option<PathBuf>,
    pub mac_address: String,
    /// The machine's folder, where an engine keeps what it has to remember about the machine.
    pub directory: PathBuf,
    /// The guest's end of its serial console.
    pub console: ConsoleEnd,
    /// Whether the guest can run virtual machines of its own.
    pub nested: bool,
    /// Folders of this computer that the guest gets, whose folders are there.
    pub shares: Vec<Share>,
}

/// Where the machine's disk is.
pub(crate) enum DiskAttachment {
    /// A raw image.
    File(PathBuf),
    /// A network block device: `nbd+unix:///disk?socket=<path>`.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Network(String),
    /// The socket of a vhost-user block device.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    VhostUser(PathBuf),
}

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

    /// Shares these folders with the running guest, and no others. The guest mounts them as
    /// [`mounts`] says.
    fn share(&self, shares: &[Share]) -> Result<()>;
}

/// The tag of the device that holds all of a machine's shares, each a folder in it, on a Mac.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const SHARES_TAG: &str = proto::NAME;

/// What the guest mounts to see `shares` in [`SHARED`], after they `changed` or not. On a Mac
/// one device holds them all, and whatever it held before is forgotten when they change.
/// cloud-hypervisor has a device for each, tagged with its name, which stays as it is.
pub(crate) fn mounts(shares: &[Share], changed: bool) -> Vec<Mount> {
    if cfg!(target_os = "macos") {
        if shares.is_empty() {
            return Vec::new();
        }
        return vec![Mount {
            tag: SHARES_TAG.into(),
            path: SHARED.into(),
            read_only: false,
            fresh: changed,
        }];
    }
    shares
        .iter()
        .map(|share| Mount {
            tag: share.name.clone(),
            path: crate::shares::guest_path(&share.name),
            read_only: share.read_only,
            fresh: false,
        })
        .collect()
}

/// The name of the socket that serves the share called `name`, on Linux.
pub(crate) fn share_socket_name(name: &str) -> String {
    format!("fs-{name}")
}

#[cfg(target_os = "macos")]
pub(crate) fn new(spec: Spec, stopped: Stopped) -> Result<Box<dyn Engine>> {
    Ok(Box::new(mac::Machine::new(spec, stopped)?))
}

#[cfg(target_os = "linux")]
pub(crate) fn new(spec: Spec, stopped: Stopped) -> Result<Box<dyn Engine>> {
    Ok(Box::new(linux::Machine::new(spec, stopped)?))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn new(_spec: Spec, _stopped: Stopped) -> Result<Box<dyn Engine>> {
    Err(Error::new(
        ErrorCode::Unsupported,
        "Machines can't be started on this system.",
    ))
}

/// Fails unless machines on this computer can run machines of their own.
pub(crate) fn check_nested() -> Result<()> {
    #[cfg(target_os = "macos")]
    if mac::nested_supported() {
        return Ok(());
    }
    Err(no_nesting())
}

/// Why machines on this computer can't run machines of their own.
pub(crate) fn no_nesting() -> Error {
    Error::new(
        ErrorCode::Unsupported,
        if cfg!(target_os = "macos") {
            "This Mac can't let a machine run machines of its own: that takes an M3 or later and macOS 15 or later."
        } else {
            "Machines on this system can't run machines of their own."
        },
    )
}

/// Has what the engine needs ready, downloading it into `cache` if it isn't there yet.
/// `progress` is told how many bytes have arrived, and of how many if the server said.
pub(crate) fn prepare(cache: &Path, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<()> {
    // Tests don't download the engine: they start no machines.
    if cfg!(target_os = "linux") && !cfg!(test) {
        tools::fetch(cache, progress)?;
    }
    Ok(())
}
