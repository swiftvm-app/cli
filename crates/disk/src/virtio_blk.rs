//! A virtio block device's requests: read from a chain of descriptors in the guest's memory,
//! carried out on a [`Disk`], and answered in the same chain.
//!
//! What a chain holds is the guest's doing, so every request is checked before it reaches the
//! disk. The vhost-user server ([`crate::vhost`]) hands chains to [`answer`]; this part needs no
//! Linux, and is tested everywhere.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::ops::Deref;

use virtio_queue::{DescriptorChain, Reader, Writer};
use vm_memory::GuestMemory;
use vm_memory::bitmap::{BitmapSlice, WithBitmapSlice};

use crate::Disk;

/// A sector, which requests count in. The disk's blocks are this size too.
pub const SECTOR: u64 = 512;
/// The largest read or write that is answered, as the NBD server does.
pub const LARGEST_REQUEST: usize = 32 << 20;
/// What a thread keeps of its buffer between requests. Most are far smaller.
const KEPT_BUFFER: usize = 1 << 20;
/// The stretches that one request to discard or zero may name.
pub const RANGES: u32 = 1;
/// The sectors that one stretch to discard or zero may cover.
pub const RANGE_SECTORS: u32 = 1 << 22;

// The features of a virtio block device that are offered, by their bits.
pub const F_SEG_MAX: u64 = 1 << 2;
pub const F_RO: u64 = 1 << 5;
pub const F_BLK_SIZE: u64 = 1 << 6;
pub const F_FLUSH: u64 = 1 << 9;
pub const F_MQ: u64 = 1 << 12;
pub const F_DISCARD: u64 = 1 << 13;
pub const F_WRITE_ZEROES: u64 = 1 << 14;
pub const F_RING_EVENT_IDX: u64 = 1 << 29;
pub const F_VERSION_1: u64 = 1 << 32;

const IN: u32 = 0;
const OUT: u32 = 1;
const FLUSH: u32 = 4;
const GET_ID: u32 = 8;
const DISCARD: u32 = 11;
const WRITE_ZEROES: u32 = 13;

const OK: u8 = 0;
const IOERR: u8 = 1;
const UNSUPP: u8 = 2;

/// The length of a device's ID, which `GET_ID` answers with.
const ID_LENGTH: usize = 20;

/// The features this device offers for `disk`, apart from the transport's own.
pub fn features(disk: &dyn Disk, queues: u16) -> u64 {
    let mut features =
        F_VERSION_1 | F_RING_EVENT_IDX | F_SEG_MAX | F_BLK_SIZE | F_FLUSH | F_DISCARD | F_WRITE_ZEROES;
    if queues > 1 {
        features |= F_MQ;
    }
    if disk.is_read_only() {
        features |= F_RO;
    }
    features
}

/// The device's configuration space, as a guest reads it: `struct virtio_blk_config`, little
/// endian.
pub fn config(disk: &dyn Disk, queues: u16, queue_size: u16) -> [u8; 60] {
    let mut config = [0; 60];
    let mut put = |at: usize, bytes: &[u8]| config[at..at + bytes.len()].copy_from_slice(bytes);
    put(0, &(disk.capacity() / SECTOR).to_le_bytes());
    // `seg_max`: the descriptors of a request, less its header and its status.
    put(12, &(u32::from(queue_size) - 2).to_le_bytes());
    put(20, &(SECTOR as u32).to_le_bytes());
    put(34, &queues.to_le_bytes());
    put(36, &RANGE_SECTORS.to_le_bytes());
    put(40, &RANGES.to_le_bytes());
    // `discard_sector_alignment`: a 4 KiB block.
    put(44, &8u32.to_le_bytes());
    put(48, &RANGE_SECTORS.to_le_bytes());
    put(52, &RANGES.to_le_bytes());
    // `write_zeroes_may_unmap`.
    put(56, &[1]);
    config
}

thread_local! {
    /// What a read or write passes through, kept for the thread's next request.
    static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Carries out the request in `chain` and writes its status into it. Returns how many bytes were
/// written into the chain, which the used ring is told.
///
/// A thread keeps the buffer of its last request, up to a size.
pub fn answer<M>(disk: &dyn Disk, chain: DescriptorChain<M>) -> u32
where
    M: Deref + Clone,
    M::Target: GuestMemory + Sized,
    for<'a> <M::Target as GuestMemory>::Bitmap: WithBitmapSlice<'a>,
{
    let memory = chain.memory();
    let (Ok(mut reader), Ok(mut writer)) = (chain.clone().reader(memory), chain.clone().writer(memory))
    else {
        return 0;
    };
    // The status is the chain's last writable byte. A chain without one can't be answered.
    let Some(before_status) = writer.available_bytes().checked_sub(1) else {
        return 0;
    };
    let Ok(mut status) = writer.split_at(before_status) else {
        return 0;
    };
    let (result, written) = carry_out(disk, &mut reader, &mut writer);
    BUFFER.with_borrow_mut(|buffer| {
        if buffer.capacity() > KEPT_BUFFER {
            buffer.truncate(KEPT_BUFFER);
            buffer.shrink_to(KEPT_BUFFER);
        }
    });
    if status.write_all(&[result]).is_err() {
        return 0;
    }
    written as u32 + 1
}

/// Returns the request's status, and how many bytes of data it wrote into the chain.
fn carry_out<B: BitmapSlice>(
    disk: &dyn Disk,
    reader: &mut Reader<'_, B>,
    writer: &mut Writer<'_, B>,
) -> (u8, usize) {
    let mut header = [0; 16];
    if reader.read_exact(&mut header).is_err() {
        return (IOERR, 0);
    }
    let kind = u32::from_le_bytes(header[..4].try_into().unwrap());
    let sector = u64::from_le_bytes(header[8..].try_into().unwrap());
    let within = |length: usize| {
        sector
            .checked_mul(SECTOR)
            .and_then(|offset| Some((offset, offset.checked_add(length as u64)?)))
            .filter(|&(_, end)| end <= disk.capacity())
            .map(|(offset, _)| offset)
    };
    let changes = matches!(kind, OUT | DISCARD | WRITE_ZEROES);
    if changes && disk.is_read_only() {
        return (IOERR, 0);
    }
    match kind {
        IN => {
            let length = writer.available_bytes();
            let Some(offset) = within(length).filter(|_| length <= LARGEST_REQUEST) else {
                return (IOERR, 0);
            };
            BUFFER.with_borrow_mut(|buffer| {
                buffer.resize(length, 0);
                if disk.read(offset, buffer).is_err() {
                    return (IOERR, 0);
                }
                match writer.write_all(buffer) {
                    Ok(()) => (OK, length),
                    Err(_) => (IOERR, 0),
                }
            })
        }
        OUT => {
            let length = reader.available_bytes();
            let Some(offset) = within(length).filter(|_| length <= LARGEST_REQUEST) else {
                return (IOERR, 0);
            };
            BUFFER.with_borrow_mut(|buffer| {
                buffer.resize(length, 0);
                if reader.read_exact(buffer).is_err() {
                    return (IOERR, 0);
                }
                (status(disk.write(offset, buffer)), 0)
            })
        }
        // A disk that is only read has nothing waiting to be written.
        FLUSH if disk.is_read_only() => (OK, 0),
        FLUSH => (status(disk.flush()), 0),
        // No serial number: zeros, which a guest takes for none.
        GET_ID => {
            let length = writer.available_bytes().min(ID_LENGTH);
            match writer.write_all(&[0; ID_LENGTH][..length]) {
                Ok(()) => (OK, length),
                Err(_) => (IOERR, 0),
            }
        }
        DISCARD | WRITE_ZEROES => {
            // `struct virtio_blk_discard_write_zeroes`, as many as were offered at most.
            let count = reader.available_bytes() / 16;
            if count == 0 || count > RANGES as usize || !reader.available_bytes().is_multiple_of(16) {
                return (IOERR, 0);
            }
            for _ in 0..count {
                let mut range = [0; 16];
                if reader.read_exact(&mut range).is_err() {
                    return (IOERR, 0);
                }
                let start = u64::from_le_bytes(range[..8].try_into().unwrap());
                let sectors = u32::from_le_bytes(range[8..12].try_into().unwrap());
                if sectors > RANGE_SECTORS {
                    return (IOERR, 0);
                }
                let length = u64::from(sectors) * SECTOR;
                let Some(offset) = start.checked_mul(SECTOR).filter(|offset| {
                    offset
                        .checked_add(length)
                        .is_some_and(|end| end <= disk.capacity())
                }) else {
                    return (IOERR, 0);
                };
                let done = if kind == DISCARD {
                    disk.discard(offset, length)
                } else {
                    disk.zero(offset, length)
                };
                if done.is_err() {
                    return (IOERR, 0);
                }
            }
            (OK, 0)
        }
        _ => (UNSUPP, 0),
    }
}

fn status(result: std::io::Result<()>) -> u8 {
    if result.is_ok() { OK } else { IOERR }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Mutex;

    use virtio_queue::desc::RawDescriptor;
    use virtio_queue::desc::split::Descriptor;
    use virtio_queue::mock::MockSplitQueue;
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    use super::*;

    const WRITABLE: u16 = 2;
    const CAPACITY: u64 = 1 << 20;

    struct Memory {
        bytes: Mutex<Vec<u8>>,
        read_only: bool,
    }

    impl Disk for Memory {
        fn capacity(&self) -> u64 {
            CAPACITY
        }
        fn is_read_only(&self) -> bool {
            self.read_only
        }
        fn read(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
            let bytes = self.bytes.lock().unwrap();
            buffer.copy_from_slice(&bytes[offset as usize..offset as usize + buffer.len()]);
            Ok(())
        }
        fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
            self.bytes.lock().unwrap()[offset as usize..offset as usize + data.len()].copy_from_slice(data);
            Ok(())
        }
        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
        fn zero(&self, offset: u64, length: u64) -> io::Result<()> {
            self.bytes.lock().unwrap()[offset as usize..(offset + length) as usize].fill(0);
            Ok(())
        }
        fn discard(&self, offset: u64, length: u64) -> io::Result<()> {
            self.zero(offset, length)
        }
    }

    fn disk(read_only: bool) -> Memory {
        Memory {
            bytes: Mutex::new(vec![0xee; CAPACITY as usize]),
            read_only,
        }
    }

    const HEADER: u64 = 0x10_0000;
    const DATA: u64 = 0x20_0000;
    const STATUS: u64 = 0x30_0000;

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x40_0000)]).unwrap()
    }

    fn header(memory: &GuestMemoryMmap, kind: u32, sector: u64) {
        let mut bytes = [0; 16];
        bytes[..4].copy_from_slice(&kind.to_le_bytes());
        bytes[8..].copy_from_slice(&sector.to_le_bytes());
        memory.write_slice(&bytes, GuestAddress(HEADER)).unwrap();
    }

    /// Puts the request in a chain: its header, `data` as descriptors of these lengths, readable or
    /// writable, and its status. Returns what [`answer`] said and the status it wrote.
    fn ask(memory: &GuestMemoryMmap, disk: &dyn Disk, data: &[(u32, bool)], status: bool) -> (u32, u8) {
        let mut descriptors = vec![RawDescriptor::from(Descriptor::new(HEADER, 16, 0, 0))];
        let mut at = DATA;
        for &(length, writable) in data {
            let flags = if writable { WRITABLE } else { 0 };
            descriptors.push(RawDescriptor::from(Descriptor::new(at, length, flags, 0)));
            at += u64::from(length);
        }
        if status {
            descriptors.push(RawDescriptor::from(Descriptor::new(STATUS, 1, WRITABLE, 0)));
        }
        memory.write_obj(0xffu8, GuestAddress(STATUS)).unwrap();
        let queue = MockSplitQueue::new(memory, 16);
        let chain = queue.build_desc_chain(&descriptors).unwrap();
        let written = answer(disk, chain);
        (written, memory.read_obj(GuestAddress(STATUS)).unwrap())
    }

    #[test]
    fn writes_are_read_back_across_descriptors() {
        let memory = memory();
        let disk = disk(false);
        let data: Vec<u8> = (0..3072).map(|byte| byte as u8).collect();
        memory.write_slice(&data, GuestAddress(DATA)).unwrap();
        header(&memory, OUT, 3);
        assert_eq!(
            ask(&memory, &disk, &[(1024, false), (2048, false)], true),
            (1, OK)
        );
        assert_eq!(disk.bytes.lock().unwrap()[3 * 512..3 * 512 + 3072], data[..]);

        memory.write_slice(&[0; 3072], GuestAddress(DATA)).unwrap();
        header(&memory, IN, 3);
        assert_eq!(
            ask(&memory, &disk, &[(512, true), (2560, true)], true),
            (3073, OK)
        );
        let mut read = vec![0; 3072];
        memory.read_slice(&mut read, GuestAddress(DATA)).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn requests_past_the_end_are_refused() {
        let memory = memory();
        let disk = disk(false);
        let last = CAPACITY / SECTOR - 1;
        header(&memory, IN, last);
        assert_eq!(ask(&memory, &disk, &[(1024, true)], true), (1, IOERR));
        header(&memory, OUT, last);
        assert_eq!(ask(&memory, &disk, &[(1024, false)], true), (1, IOERR));
        header(&memory, IN, u64::MAX / 2);
        assert_eq!(ask(&memory, &disk, &[(512, true)], true), (1, IOERR));
        // The last sector itself is fine.
        header(&memory, IN, last);
        assert_eq!(ask(&memory, &disk, &[(512, true)], true), (513, OK));
    }

    #[test]
    fn a_disk_that_is_only_read_is_not_changed() {
        let memory = memory();
        let disk = disk(true);
        header(&memory, OUT, 0);
        assert_eq!(ask(&memory, &disk, &[(512, false)], true), (1, IOERR));
        header(&memory, FLUSH, 0);
        assert_eq!(ask(&memory, &disk, &[], true), (1, OK));
        assert!(disk.bytes.lock().unwrap().iter().all(|&byte| byte == 0xee));
        assert_ne!(features(&disk, 1) & F_RO, 0);
    }

    #[test]
    fn stretches_are_discarded_and_zeroed() {
        let memory = memory();
        let disk = disk(false);
        let range = |sector: u64, sectors: u32| {
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(&sector.to_le_bytes());
            bytes[8..12].copy_from_slice(&sectors.to_le_bytes());
            bytes
        };
        for (kind, sector) in [(DISCARD, 2), (WRITE_ZEROES, 10)] {
            memory.write_slice(&range(sector, 4), GuestAddress(DATA)).unwrap();
            header(&memory, kind, 0);
            assert_eq!(ask(&memory, &disk, &[(16, false)], true), (1, OK));
            let bytes = disk.bytes.lock().unwrap();
            let start = sector as usize * 512;
            assert!(bytes[start..start + 2048].iter().all(|&byte| byte == 0));
            assert_eq!(bytes[start + 2048], 0xee);
            assert_eq!(bytes[start - 1], 0xee);
        }
        // More stretches than were offered, a stretch past the end, and one that isn't whole.
        memory
            .write_slice(&[range(0, 1), range(4, 1)].concat(), GuestAddress(DATA))
            .unwrap();
        header(&memory, DISCARD, 0);
        assert_eq!(ask(&memory, &disk, &[(32, false)], true), (1, IOERR));
        memory
            .write_slice(&range(CAPACITY / SECTOR - 1, 2), GuestAddress(DATA))
            .unwrap();
        assert_eq!(ask(&memory, &disk, &[(16, false)], true), (1, IOERR));
        assert_eq!(ask(&memory, &disk, &[(12, false)], true), (1, IOERR));
    }

    #[test]
    fn other_requests() {
        let memory = memory();
        let disk = disk(false);
        header(&memory, GET_ID, 0);
        memory.write_slice(&[0xaa; 20], GuestAddress(DATA)).unwrap();
        assert_eq!(ask(&memory, &disk, &[(20, true)], true), (21, OK));
        let mut id = [0xaa; 20];
        memory.read_slice(&mut id, GuestAddress(DATA)).unwrap();
        assert_eq!(id, [0; 20]);
        header(&memory, 99, 0);
        assert_eq!(ask(&memory, &disk, &[], true), (1, UNSUPP));
        // Without room for a status, there is no answer to give.
        header(&memory, IN, 0);
        assert_eq!(ask(&memory, &disk, &[(512, false)], false), (0, 0xff));
    }

    #[test]
    fn the_configuration_describes_the_disk() {
        let disk = disk(false);
        let config = config(&disk, 4, 256);
        assert_eq!(
            u64::from_le_bytes(config[..8].try_into().unwrap()),
            CAPACITY / 512
        );
        assert_eq!(u32::from_le_bytes(config[12..16].try_into().unwrap()), 254);
        assert_eq!(u32::from_le_bytes(config[20..24].try_into().unwrap()), 512);
        assert_eq!(u16::from_le_bytes(config[34..36].try_into().unwrap()), 4);
        assert_ne!(features(&disk, 4) & F_MQ, 0);
        assert_eq!(features(&disk, 1) & (F_MQ | F_RO), 0);
    }
}
