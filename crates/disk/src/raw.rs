use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::Disk;

/// A disk whose bytes are a file's bytes.
pub struct RawDisk {
    file: File,
    capacity: u64,
    read_only: bool,
}

impl RawDisk {
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let capacity = file.metadata()?.len();
        Ok(Self {
            file,
            capacity,
            read_only,
        })
    }
}

impl Disk for RawDisk {
    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn read(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buffer, offset)
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.write_all_at(data, offset)
    }

    fn flush(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn zero(&self, offset: u64, length: u64) -> io::Result<()> {
        let zeros = [0u8; 65536];
        let mut done = 0;
        while done < length {
            let count = (length - done).min(zeros.len() as u64) as usize;
            self.file.write_all_at(&zeros[..count], offset + done)?;
            done += count as u64;
        }
        Ok(())
    }

    fn discard(&self, _offset: u64, _length: u64) -> io::Result<()> {
        Ok(())
    }
}
