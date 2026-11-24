//! A disk whose contents can be moved to another disk while it is served: a machine's layer is
//! sealed and a new one put on top without the guest noticing.

use std::io;
use std::sync::{Arc, RwLock};

use crate::Disk;

/// Passes each request on to the disk it holds at that moment. A switch waits for the requests
/// under way, and those that arrive meanwhile wait for it.
pub struct Switchable<D> {
    current: RwLock<Arc<D>>,
}

impl<D: Disk> Switchable<D> {
    pub fn new(disk: Arc<D>) -> Self {
        Self {
            current: RwLock::new(disk),
        }
    }

    /// The disk that requests go to now.
    pub fn current(&self) -> Arc<D> {
        self.current.read().unwrap().clone()
    }

    /// Hands the disk to `switch`, with nothing reading or writing it, and serves the one it
    /// returns from then on. One of another size is refused. When `switch` fails, the disk it
    /// was given stays, whatever it did to it.
    pub fn switch(&self, switch: impl FnOnce(&Arc<D>) -> io::Result<Arc<D>>) -> io::Result<()> {
        let mut current = self.current.write().unwrap();
        let next = switch(&current)?;
        if next.capacity() != current.capacity() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a disk can only be switched for one of the same size",
            ));
        }
        *current = next;
        Ok(())
    }
}

impl<D: Disk> Disk for Switchable<D> {
    fn capacity(&self) -> u64 {
        self.current.read().unwrap().capacity()
    }

    fn is_read_only(&self) -> bool {
        self.current.read().unwrap().is_read_only()
    }

    fn read(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        self.current.read().unwrap().read(offset, buffer)
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.current.read().unwrap().write(offset, data)
    }

    fn flush(&self) -> io::Result<()> {
        self.current.read().unwrap().flush()
    }

    fn zero(&self, offset: u64, length: u64) -> io::Result<()> {
        self.current.read().unwrap().zero(offset, length)
    }

    fn discard(&self, offset: u64, length: u64) -> io::Result<()> {
        self.current.read().unwrap().discard(offset, length)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::layer::{Layer, hash_file};
    use crate::{Header, Options};

    fn folder(test: &str) -> std::path::PathBuf {
        let random: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let folder = std::env::temp_dir().join(format!("switch-test-{test}-{random:x}"));
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    /// Writers keep going while the layer they write is sealed and a new one is put on top of
    /// it. Every write lands in one of the two, and the disk reads back as it was written.
    #[test]
    fn writes_carry_on_across_a_switch() {
        const CAPACITY: u64 = 8 << 20;
        let folder = folder("writers");
        let first = folder.join("first.layer");
        Layer::create(&first, CAPACITY, None).unwrap();
        let options = Options::default();
        let disk = Arc::new(Switchable::new(Arc::new(
            Layer::open(&first, &options, None).unwrap(),
        )));

        let stop = Arc::new(AtomicBool::new(false));
        let writers: Vec<_> = (0..4u64)
            .map(|writer| {
                let disk = disk.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    // Each writer has its own stretch of the disk, and writes rounds into it.
                    let mut round = 0u8;
                    let stretch = CAPACITY / 4;
                    while !stop.load(Ordering::SeqCst) || round < 3 {
                        round = round.wrapping_add(1);
                        let block = vec![round ^ writer as u8; 4096];
                        for offset in (0..stretch).step_by(65536) {
                            disk.write(writer * stretch + offset, &block).unwrap();
                        }
                    }
                    round
                })
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(50));

        let second = folder.join("second.layer");
        disk.switch(|old| {
            old.seal()?;
            let hash = hash_file(&first)?;
            Layer::create(&second, CAPACITY, Some(hash))?;
            let parent = Arc::new(Layer::open(&first, &Options::read_only(), None)?);
            Ok(Arc::new(Layer::open(&second, &options, Some(parent))?))
        })
        .unwrap();
        assert!(Header::read(&first).unwrap().sealed);
        std::thread::sleep(std::time::Duration::from_millis(50));
        stop.store(true, Ordering::SeqCst);
        let rounds: Vec<u8> = writers.into_iter().map(|writer| writer.join().unwrap()).collect();

        let stretch = CAPACITY / 4;
        let mut read = vec![0; 4096];
        for (writer, round) in rounds.iter().enumerate() {
            for offset in (0..stretch).step_by(65536) {
                disk.read(writer as u64 * stretch + offset, &mut read).unwrap();
                assert!(read.iter().all(|&byte| byte == round ^ writer as u8));
            }
        }
        disk.current().close().unwrap();
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_failed_switch_keeps_the_disk() {
        let folder = folder("failed");
        let path = folder.join("a.layer");
        Layer::create(&path, 1 << 20, None).unwrap();
        let disk = Switchable::new(Arc::new(Layer::open(&path, &Options::default(), None).unwrap()));
        disk.write(0, &[7; 512]).unwrap();
        let failed = disk.switch(|_| Err(io::Error::other("no")));
        assert!(failed.is_err());
        let other = folder.join("b.layer");
        Layer::create(&other, 2 << 20, None).unwrap();
        let larger = disk.switch(|_| Ok(Arc::new(Layer::open(&other, &Options::default(), None)?)));
        assert_eq!(larger.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        let mut read = [0; 512];
        disk.read(0, &mut read).unwrap();
        assert_eq!(read, [7; 512]);
        disk.current().close().unwrap();
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
