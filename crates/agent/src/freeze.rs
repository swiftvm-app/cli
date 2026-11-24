//! Keeps the guest's root file system from changing while the host takes its disk as it is.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use proto::agent::Answer;
use proto::{Error, ErrorCode};

#[cfg(target_os = "linux")]
mod system {
    use std::io;
    use std::os::fd::AsRawFd;

    // _IOWR('X', 119, int) and _IOWR('X', 120, int).
    const FIFREEZE: libc::c_ulong = 0xC004_5877;
    const FITHAW: libc::c_ulong = 0xC004_5878;

    pub struct Frozen(std::fs::File);

    fn ioctl(root: &std::fs::File, request: libc::c_ulong) -> io::Result<()> {
        // Safety: the descriptor is open, and these requests take no argument that is read.
        if unsafe { libc::ioctl(root.as_raw_fd(), request as _, 0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn freeze() -> io::Result<Frozen> {
        // Safety: a plain system call.
        unsafe { libc::sync() };
        let root = std::fs::File::open("/")?;
        ioctl(&root, FIFREEZE)?;
        Ok(Frozen(root))
    }

    impl Drop for Frozen {
        fn drop(&mut self) {
            _ = ioctl(&self.0, FITHAW);
        }
    }

    /// Thaws the root file system, which an agent that ended while it was frozen left so.
    pub fn thaw() {
        if let Ok(root) = std::fs::File::open("/") {
            _ = ioctl(&root, FITHAW);
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod system {
    pub struct Frozen;

    pub fn freeze() -> std::io::Result<Frozen> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "only Linux's file systems can be frozen",
        ))
    }
}

#[cfg(target_os = "linux")]
pub use system::thaw;

/// Freezes the root file system, says so, and thaws it once the host hangs up or `seconds`
/// have passed. Nothing is written meanwhile: the agent's own messages would wait for the disk.
pub fn run(mut stream: UnixStream, seconds: u64) {
    // Thawed when this returns.
    let _frozen = match system::freeze() {
        Ok(frozen) => frozen,
        Err(error) => {
            let error = Error::new(
                ErrorCode::Guest,
                format!("Couldn't freeze the guest's file system: {error}."),
            );
            _ = proto::send(&mut stream, &Answer::no(error));
            return;
        }
    };
    if proto::send(&mut stream, &Answer::yes()).is_ok() {
        _ = stream.set_read_timeout(Some(Duration::from_secs(seconds.max(1))));
        // Whatever arrives, the end of the connection or the time running out ends it.
        _ = stream.read(&mut [0; 1]);
    }
}
