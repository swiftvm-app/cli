//! Where a layer keeps its bytes: a file, or in tests something that can lose what the system
//! hadn't written out yet.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};

/// The bytes of a layer, and what a layer needs to know about when they are safe.
pub trait Storage: Send + Sync + 'static {
    /// Reads from `offset` on, and says how much that was: less at the end.
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize>;

    fn write_all_at(&self, data: &[u8], offset: u64) -> io::Result<()>;

    fn length(&self) -> io::Result<u64>;

    fn set_len(&self, length: u64) -> io::Result<()>;

    /// Returns once everything written so far survives a crash of the host.
    fn sync(&self) -> io::Result<()>;

    /// Makes everything written so far reach the disk before anything written after. Doesn't wait
    /// for it to get there.
    fn barrier(&self) -> io::Result<()>;

    /// Gives back the room that a part takes up. It reads as zeros afterwards. Only tidying: a
    /// storage that can't is left as it is.
    fn punch(&self, offset: u64, length: u64);

    /// The bytes it takes up on the host.
    fn allocated(&self) -> u64;
}

/// A layer in a file.
pub struct FileStorage(pub File);

impl Storage for FileStorage {
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
        let mut done = 0;
        while done < buffer.len() {
            match self.0.read_at(&mut buffer[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(count) => done += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(done)
    }

    fn write_all_at(&self, data: &[u8], offset: u64) -> io::Result<()> {
        self.0.write_all_at(data, offset)
    }

    fn length(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }

    fn set_len(&self, length: u64) -> io::Result<()> {
        self.0.set_len(length)
    }

    fn sync(&self) -> io::Result<()> {
        // A Mac's own fsync leaves what it wrote in the drive's cache. A file system that doesn't
        // know the stronger call is asked the ordinary way.
        #[cfg(target_os = "macos")]
        // Safety: the descriptor is open while the file is borrowed.
        if unsafe { libc::fcntl(self.0.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
        // Not `sync_all`, which is the stronger call again on a Mac.
        // Safety: as above.
        if unsafe { libc::fsync(self.0.as_raw_fd()) } == 0 {
            return Ok(());
        }
        Err(io::Error::last_os_error())
    }

    fn barrier(&self) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        // Safety: as above.
        if unsafe { libc::fcntl(self.0.as_raw_fd(), libc::F_BARRIERFSYNC) } == 0 {
            return Ok(());
        }
        // Linux has no barrier on its own: waiting for the data is one.
        self.0.sync_data()
    }

    fn punch(&self, offset: u64, length: u64) {
        if length == 0 {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            let hole = libc::fpunchhole_t {
                fp_flags: 0,
                reserved: 0,
                fp_offset: offset as libc::off_t,
                fp_length: length as libc::off_t,
            };
            // Safety: the descriptor is open, and the call reads the structure only.
            unsafe { libc::fcntl(self.0.as_raw_fd(), libc::F_PUNCHHOLE, &raw const hole) };
        }
        #[cfg(target_os = "linux")]
        // Safety: as above.
        unsafe {
            libc::fallocate(
                self.0.as_raw_fd(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                offset as libc::off_t,
                length as libc::off_t,
            );
        }
    }

    fn allocated(&self) -> u64 {
        // Counted in units of 512 bytes, whatever the file system's own are.
        self.0.metadata().map_or(0, |metadata| metadata.blocks() * 512)
    }
}
