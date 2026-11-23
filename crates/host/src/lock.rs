//! Locks between processes, as files. The system lets go of one when the process that holds it
//! ends, however it ends.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

/// A lock that is held until this is dropped.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

fn flock(file: &File, operation: libc::c_int) -> io::Result<bool> {
    loop {
        // Safety: the descriptor is open for as long as the file is borrowed.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::WouldBlock => return Ok(false),
            io::ErrorKind::Interrupted => {}
            _ => return Err(error),
        }
    }
}

fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
}

impl Lock {
    /// Waits for the lock.
    pub fn take(path: &Path) -> io::Result<Self> {
        let file = open(path)?;
        flock(&file, libc::LOCK_EX)?;
        Ok(Self { _file: file })
    }

    /// Takes the lock unless another process holds it.
    pub fn try_take(path: &Path) -> io::Result<Option<Self>> {
        let file = open(path)?;
        Ok(flock(&file, libc::LOCK_EX | libc::LOCK_NB)?.then_some(Self { _file: file }))
    }

    /// Takes the lock, waiting for `limit` at most. Someone who only looks whether it is held
    /// ([`Lock::is_held`]) is in the way for a moment, which this waits out.
    pub fn take_within(path: &Path, limit: Duration) -> io::Result<Option<Self>> {
        let deadline = Instant::now() + limit;
        loop {
            match Self::try_take(path)? {
                Some(lock) => return Ok(Some(lock)),
                None if Instant::now() >= deadline => return Ok(None),
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// Whether a process holds the lock. Looking doesn't keep anyone else from looking, and a
    /// lock that was never taken has no file.
    pub fn is_held(path: &Path) -> bool {
        match File::open(path) {
            Ok(file) => !flock(&file, libc::LOCK_SH | libc::LOCK_NB).unwrap_or(true),
            Err(_) => false,
        }
    }
}
