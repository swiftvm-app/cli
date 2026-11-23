//! Virtual disks, and the servers that hand them to a virtualization engine.

pub mod nbd;
mod raw;

pub use raw::RawDisk;

use std::io;

/// A disk as a guest sees it: a run of bytes that can be read and written anywhere.
///
/// Requests arrive from several threads at once. Each is within the disk's capacity: a server
/// checks that before it asks.
pub trait Disk: Send + Sync + 'static {
    /// The size of the disk in bytes.
    fn capacity(&self) -> u64;

    fn is_read_only(&self) -> bool;

    fn read(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()>;

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()>;

    /// Returns once everything written so far survives a crash of the host.
    fn flush(&self) -> io::Result<()>;

    /// Fills a part of the disk with zeros.
    fn zero(&self, offset: u64, length: u64) -> io::Result<()>;

    /// Lets go of a part of the disk that the guest no longer needs. What it reads there
    /// afterwards is up to the disk.
    fn discard(&self, offset: u64, length: u64) -> io::Result<()>;
}
