//! A layer: a disk that keeps its contents compressed in one file, on top of another layer or of
//! nothing.
//!
//! The disk is cut into clusters of 64 KiB. Each is stored compressed with zstd, as it is when
//! compressing doesn't help, or not at all: a map entry says that a cluster is zeros, or that it
//! is whatever the layer underneath holds there. A layer with nothing underneath reads zeros there.
//!
//! Nothing in the file is ever overwritten: a cluster that changes is written to free space, and
//! the map follows. The map is committed once a second, by writing its changed pages to free space
//! as well and then one block that points to them. So the file holds a complete disk at every
//! moment, as it was at the last commit, whatever happens to the host.
//!
//! A guest is told that what it has written is safe, and it can't say when it means it: Apple's
//! framework passes no flushes on. So every change to the map is also noted in a journal before the
//! guest hears that its write is done. The journal isn't synchronized, which would cost a write its
//! speed. When the program goes down, the system still has all of it and nothing is lost. When the
//! host goes down, the journal is followed for as far as the data it speaks of proves to be there,
//! and the disk comes back as it was at one moment in its last second.
//!
//! A sealed layer is never written again, and is only opened for reading. Layers built on it name
//! it by the SHA-256 of its file.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::Disk;
use crate::allocator::{Allocator, Run};
use crate::storage::{FileStorage, Storage};

#[cfg(test)]
mod tests;

pub const BLOCK: usize = 4096;
pub const CLUSTER: usize = 65536;
const ENTRY_BYTES: usize = 16;
const ENTRIES_PER_PAGE: usize = BLOCK / ENTRY_BYTES;
/// Blocks in each of the two halves of the journal.
const JOURNAL_BLOCKS: usize = 4096;
const RECORD: usize = 32;
const JOURNAL_BYTES: usize = JOURNAL_BLOCKS * BLOCK;
/// Past this a commit is asked for early.
const JOURNAL_SOFT_LIMIT: usize = JOURNAL_BYTES / 2;
/// Past this writes wait for one. The rest is room for the writes that are already on their way.
const JOURNAL_HARD_LIMIT: usize = JOURNAL_BYTES * 3 / 4;
/// A block behind the journal that is written once and never let go of. APFS fills a hole that
/// reaches the end of a file as soon as something is written into it, so the journal's empty
/// space needs a block that is in use behind it, also in a layer that holds nothing yet.
const GUARD_BLOCK: usize = 2 + 2 * JOURNAL_BLOCKS;
/// Two blocks that point to the map, the journal and its guard. Everything after is handed out
/// as needed.
const FIRST_FREE_BLOCK: usize = GUARD_BLOCK + 1;
/// Free blocks in a row that are given back to the host as soon as they are free: a megabyte.
const LARGE_RUN: usize = 256;
const STRIPES: usize = 1024;
const VERSION: u32 = 1;
const MAGIC: [u8; 8] = *b"ZSTDLAYR";
/// The flag of a sealed layer, in its header.
const SEALED: u32 = 1;
/// How hard clusters are compressed unless said: zstd's own default.
pub const DEFAULT_LEVEL: i32 = 3;

/// What a map entry says about its cluster.
mod kind {
    /// Whatever the layer underneath holds, zeros without one. A new layer is all of this.
    pub const PARENT: u8 = 0;
    pub const ZEROS: u8 = 1;
    pub const RAW: u8 = 2;
    pub const ZSTD: u8 = 3;
    /// In the journal: the map was frozen for a commit here, and the journal goes on in its
    /// other half.
    pub const END_OF_EPOCH: u8 = 255;
}

/// Where a cluster is in the file, and what its stored bytes add up to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Entry {
    block: u32,
    /// The stored length in bytes, and the kind in the top byte.
    info: u32,
    checksum: u32,
}

impl Entry {
    fn new(block: usize, length: usize, kind: u8, checksum: u32) -> Self {
        Self {
            block: block as u32,
            info: length as u32 | (kind as u32) << 24,
            checksum,
        }
    }

    fn kind(&self) -> u8 {
        (self.info >> 24) as u8
    }

    fn length(&self) -> usize {
        (self.info & 0xFF_FFFF) as usize
    }

    fn block_count(&self) -> usize {
        self.length().div_ceil(BLOCK)
    }

    /// Whether it has bytes of its own in the file.
    fn is_stored(&self) -> bool {
        matches!(self.kind(), kind::RAW | kind::ZSTD)
    }

    fn offset(&self) -> u64 {
        self.block as u64 * BLOCK as u64
    }

    fn run(&self) -> Run {
        Run {
            block: self.block as usize,
            count: self.block_count(),
        }
    }
}

fn corrupted(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the layer is damaged: {what}"),
    )
}

fn os(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

fn checksum(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

fn is_zero(bytes: &[u8]) -> bool {
    // A fold rather than a search, which the compiler makes wide.
    bytes
        .chunks(4096)
        .all(|chunk| chunk.iter().fold(0, |all, &byte| all | byte) == 0)
}

fn cluster_count(capacity: u64) -> usize {
    capacity.div_ceil(CLUSTER as u64) as usize
}

fn page_count(capacity: u64) -> usize {
    cluster_count(capacity).div_ceil(ENTRIES_PER_PAGE)
}

fn journal_offset(base: u64) -> u64 {
    ((2 + (base % 2) as usize * JOURNAL_BLOCKS) * BLOCK) as u64
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_exact(storage: &dyn Storage, buffer: &mut [u8], offset: u64) -> io::Result<()> {
    if storage.read_at(buffer, offset)? < buffer.len() {
        return Err(corrupted("it is cut short"));
    }
    Ok(())
}

/// The map, as entries that can be changed while it is shared. Each cluster's stripe lock keeps
/// its entry's fields together.
struct Map {
    capacity: u64,
    clusters: usize,
    pages: usize,
    /// The block and the info of each entry.
    places: Vec<AtomicU64>,
    checksums: Vec<AtomicU32>,
}

impl Map {
    fn new(capacity: u64) -> Self {
        let mut map = Self {
            capacity: 0,
            clusters: 0,
            pages: 0,
            places: Vec::new(),
            checksums: Vec::new(),
        };
        map.grow(capacity);
        map
    }

    fn grow(&mut self, capacity: u64) {
        self.capacity = capacity;
        self.clusters = cluster_count(capacity);
        self.pages = page_count(capacity);
        let entries = self.pages * ENTRIES_PER_PAGE;
        self.places.resize_with(entries, || AtomicU64::new(0));
        self.checksums.resize_with(entries, || AtomicU32::new(0));
    }

    fn get(&self, cluster: usize) -> Entry {
        let place = self.places[cluster].load(Ordering::Relaxed);
        Entry {
            block: (place >> 32) as u32,
            info: place as u32,
            checksum: self.checksums[cluster].load(Ordering::Relaxed),
        }
    }

    fn set(&self, cluster: usize, entry: Entry) {
        self.places[cluster].store((entry.block as u64) << 32 | entry.info as u64, Ordering::Relaxed);
        self.checksums[cluster].store(entry.checksum, Ordering::Relaxed);
    }

    fn page_bytes(&self, page: usize) -> Vec<u8> {
        let mut bytes = vec![0; BLOCK];
        for index in 0..ENTRIES_PER_PAGE {
            let entry = self.get(page * ENTRIES_PER_PAGE + index);
            let at = index * ENTRY_BYTES;
            put_u32(&mut bytes, at, entry.block);
            put_u32(&mut bytes, at + 4, entry.info);
            put_u32(&mut bytes, at + 8, entry.checksum);
        }
        bytes
    }

    fn load_page(&self, page: usize, bytes: &[u8]) {
        for index in 0..ENTRIES_PER_PAGE {
            let at = index * ENTRY_BYTES;
            let entry = Entry {
                block: u32_at(bytes, at),
                info: u32_at(bytes, at + 4),
                checksum: u32_at(bytes, at + 8),
            };
            self.set(page * ENTRIES_PER_PAGE + index, entry);
        }
    }
}

/// One of the two blocks that point to the map.
#[derive(Debug, Clone, Copy)]
struct Superblock {
    capacity: u64,
    generation: u64,
    table_block: u32,
    table_blocks: u32,
    table_checksum: u32,
    pages: u32,
    flags: u32,
    /// The hash of the layer underneath, zeros without one.
    parent: [u8; 32],
}

impl Superblock {
    fn encode(&self) -> Vec<u8> {
        let mut block = vec![0; BLOCK];
        block[..8].copy_from_slice(&MAGIC);
        put_u32(&mut block, 8, VERSION);
        put_u32(&mut block, 12, CLUSTER as u32);
        put_u64(&mut block, 16, self.capacity);
        put_u64(&mut block, 24, self.generation);
        put_u32(&mut block, 32, self.table_block);
        put_u32(&mut block, 36, self.table_blocks);
        put_u32(&mut block, 40, self.table_checksum);
        put_u32(&mut block, 44, self.pages);
        put_u32(&mut block, 48, JOURNAL_BLOCKS as u32);
        put_u32(&mut block, 52, self.flags);
        block[56..88].copy_from_slice(&self.parent);
        let sum = checksum(&block[..BLOCK - 4]);
        put_u32(&mut block, BLOCK - 4, sum);
        block
    }

    /// The newer of the two, of those that are whole.
    fn newest(storage: &dyn Storage) -> io::Result<Self> {
        let mut newest: Option<Self> = None;
        let mut saw_magic = false;
        let mut block = vec![0; BLOCK];
        for slot in 0..2 {
            if storage.read_at(&mut block, (slot * BLOCK) as u64)? < BLOCK || block[..8] != MAGIC {
                continue;
            }
            saw_magic = true;
            if u32_at(&block, BLOCK - 4) != checksum(&block[..BLOCK - 4]) {
                continue;
            }
            let version = u32_at(&block, 8);
            if version != VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the layer was made by a newer version (format {version})"),
                ));
            }
            if u32_at(&block, 12) != CLUSTER as u32 || u32_at(&block, 48) != JOURNAL_BLOCKS as u32 {
                return Err(corrupted("it is laid out in a way this version doesn't know"));
            }
            let superblock = Self {
                capacity: u64_at(&block, 16),
                generation: u64_at(&block, 24),
                table_block: u32_at(&block, 32),
                table_blocks: u32_at(&block, 36),
                table_checksum: u32_at(&block, 40),
                pages: u32_at(&block, 44),
                flags: u32_at(&block, 52),
                parent: block[56..88].try_into().unwrap(),
            };
            if newest.is_none_or(|newest| superblock.generation > newest.generation) {
                newest = Some(superblock);
            }
        }
        newest.ok_or_else(|| {
            if saw_magic {
                corrupted("neither copy of its header is whole")
            } else {
                io::Error::new(io::ErrorKind::InvalidData, "the file isn't a layer")
            }
        })
    }

    /// Where each page of the map is, and what it adds up to.
    fn read_table(&self, storage: &dyn Storage) -> io::Result<Vec<(u32, u32)>> {
        let mut table = vec![(0, 0); self.pages as usize];
        if self.table_blocks == 0 {
            return Ok(table);
        }
        let count = self.table_blocks as usize * BLOCK;
        if count < table.len() * 8 {
            return Err(corrupted("its map is cut short"));
        }
        let mut encoded = vec![0; count];
        read_exact(storage, &mut encoded, self.table_block as u64 * BLOCK as u64)?;
        if checksum(&encoded) != self.table_checksum {
            return Err(corrupted("its map doesn't match its checksum"));
        }
        for (index, page) in table.iter_mut().enumerate() {
            *page = (u32_at(&encoded, index * 8), u32_at(&encoded, index * 8 + 4));
        }
        Ok(table)
    }
}

/// What a layer's file says about it, read without opening it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The size of the disk in bytes.
    pub capacity: u64,
    /// The hash of the layer it is built on.
    pub parent: Option<[u8; 32]>,
    pub sealed: bool,
}

impl Header {
    pub fn read(path: &Path) -> io::Result<Self> {
        let storage = FileStorage(std::fs::File::open(path)?);
        Ok(Superblock::newest(&storage)?.into())
    }
}

impl From<Superblock> for Header {
    fn from(superblock: Superblock) -> Self {
        Self {
            capacity: superblock.capacity,
            parent: (superblock.parent != [0; 32]).then_some(superblock.parent),
            sealed: superblock.flags & SEALED != 0,
        }
    }
}

/// How a layer is opened.
#[derive(Debug, Clone)]
pub struct Options {
    pub read_only: bool,
    /// How often the map is committed. `None` leaves that to [`Disk::flush`] and
    /// [`Layer::close`].
    pub commit_interval: Option<Duration>,
    /// The zstd level that clusters are written at.
    pub level: i32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            read_only: false,
            commit_interval: Some(Duration::from_secs(1)),
            level: DEFAULT_LEVEL,
        }
    }
}

impl Options {
    pub fn read_only() -> Self {
        Self {
            read_only: true,
            commit_interval: None,
            ..Self::default()
        }
    }
}

/// What a layer holds, cluster by cluster.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// The size of the disk in bytes.
    pub capacity: u64,
    pub clusters: u64,
    /// Clusters with data in this layer.
    pub stored: u64,
    /// Clusters that this layer says are zeros, over what is underneath.
    pub zeros: u64,
    /// Clusters that read what is underneath: zeros, in a layer that has nothing underneath.
    pub inherited: u64,
    /// What the stored clusters take up, compressed.
    pub stored_bytes: u64,
    /// What the file takes up on the host.
    pub file_bytes: u64,
}

/// Where a commit can be made to fail, to see that the layer survives it.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitStep {
    MapWritten,
    MapSynchronized,
    SuperblockWritten,
}

/// The journal as it is being written, and what a commit takes along when it freezes the map.
struct Journal {
    /// The commit that the lines of this half come after.
    base: u64,
    /// Bytes of this half that are spoken for, in the file or not yet.
    tail: usize,
    /// Lines that wait to be written, and where in the half they go.
    waiting: Vec<u8>,
    waiting_start: usize,
    /// Bytes of this half that are in the file.
    written: usize,
    being_written: bool,
    /// Where this half has a gap, after lines that couldn't be written. What lies behind a gap is
    /// never followed, so no write is called done from there on until a commit has started the
    /// journal anew.
    broken_at: Option<usize>,
    dirty_pages: BTreeSet<usize>,
    /// What a change to the map let go of. Kept until the commit that no longer needs it.
    pending_free: Vec<Run>,
}

impl Journal {
    fn new(base: u64) -> Self {
        Self {
            base,
            tail: 0,
            waiting: Vec::new(),
            waiting_start: 0,
            written: 0,
            being_written: false,
            broken_at: None,
            dirty_pages: BTreeSet::new(),
            pending_free: Vec::new(),
        }
    }
}

/// A line of the journal: a cluster's new entry.
struct Record {
    /// The commit this change comes after.
    base: u64,
    cluster: u32,
    entry: Entry,
    /// Which line of its half of the journal this is.
    sequence: u32,
}

impl Record {
    fn encode(&self) -> [u8; RECORD] {
        let mut bytes = [0; RECORD];
        put_u64(&mut bytes, 0, self.base);
        put_u32(&mut bytes, 8, self.cluster);
        put_u32(&mut bytes, 12, self.entry.block);
        put_u32(&mut bytes, 16, self.entry.info);
        put_u32(&mut bytes, 20, self.entry.checksum);
        put_u32(&mut bytes, 24, self.sequence);
        let sum = checksum(&bytes[..28]);
        put_u32(&mut bytes, 28, sum);
        bytes
    }

    /// The line in `bytes`, or `None` when it isn't whole.
    fn decode(bytes: &[u8; RECORD]) -> Option<Self> {
        (u32_at(bytes, 28) == checksum(&bytes[..28])).then(|| Self {
            base: u64_at(bytes, 0),
            cluster: u32_at(bytes, 8),
            entry: Entry {
                block: u32_at(bytes, 12),
                info: u32_at(bytes, 16),
                checksum: u32_at(bytes, 20),
            },
            sequence: u32_at(bytes, 24),
        })
    }
}

/// A commit between freezing the map and having it safely in the file. Kept until it gets there:
/// the journal has moved on, so the same commit is tried again rather than a new one begun.
struct PendingCommit {
    generation: u64,
    pages: Vec<(usize, Vec<u8>)>,
    frees: Vec<Run>,
    capacity: u64,
    page_count: usize,
    flags: u32,
}

/// What only the one who commits touches.
struct Commits {
    generation: u64,
    /// Where each page of the map is in the file, and its checksum.
    table: Vec<(u32, u32)>,
    table_extent: Run,
    pending: Option<PendingCommit>,
    flags: u32,
    fault: Option<CommitStep>,
}

struct Scratch {
    plain: Vec<u8>,
    packed: Vec<u8>,
    compressor: zstd::bulk::Compressor<'static>,
    decompressor: zstd::bulk::Decompressor<'static>,
}

/// What wakes the thread that commits.
struct Wake {
    asked: bool,
    stopping: bool,
}

struct Inner {
    storage: Box<dyn Storage>,
    read_only: bool,
    parent: Option<Arc<Layer>>,
    /// Clusters of the layer underneath, which can't change: it is only read.
    parent_clusters: usize,
    parent_hash: Option<[u8; 32]>,
    level: i32,
    closed: AtomicBool,
    /// Taken for writing only to grow the layer.
    map: RwLock<Map>,
    stripes: Box<[Mutex<()>]>,
    /// Changes to the map hold this shared. A commit takes it alone for the moment it freezes the
    /// map.
    commit_lock: RwLock<()>,
    allocator: Mutex<Allocator>,
    /// Guards the journal and every change to the map that it notes.
    journal: Mutex<Journal>,
    journal_written: Condvar,
    scratch: Mutex<Vec<Scratch>>,
    /// Held by whoever commits: one at a time.
    commits: Mutex<Commits>,
    wake: Mutex<Wake>,
    woken: Condvar,
    /// Makes writing to the journal fail, to see that no write is called done without its line.
    journal_fault: AtomicBool,
    replayed: usize,
}

/// A layer that is open. Dropped without [`Layer::close`], it is left as a crash of the program
/// leaves it: the journal has the rest.
pub struct Layer {
    inner: Arc<Inner>,
    committer: Mutex<Option<JoinHandle<()>>>,
}

/// Writes an empty layer of `capacity` bytes, on top of the layer whose hash is `parent`.
fn format(storage: &dyn Storage, capacity: u64, parent: Option<[u8; 32]>) -> io::Result<()> {
    let superblock = Superblock {
        capacity,
        generation: 0,
        table_block: 0,
        table_blocks: 0,
        table_checksum: 0,
        pages: page_count(capacity) as u32,
        flags: 0,
        parent: parent.unwrap_or_default(),
    };
    // Made longer rather than written to at its end: only that is sure to leave the space between
    // empty.
    storage.set_len((FIRST_FREE_BLOCK * BLOCK) as u64)?;
    storage.write_all_at(&superblock.encode(), 0)?;
    storage.write_all_at(&[0; BLOCK], (GUARD_BLOCK * BLOCK) as u64)?;
    storage.sync()
}

struct Replay {
    changes: usize,
    pages: BTreeSet<usize>,
    /// What the lines that were followed replaced: of the last commit, or of a line before them.
    /// The file falls back on all of it if this open doesn't get as far as its own commit.
    superseded: Vec<Run>,
}

/// Follows the journal from the last commit, and stops at the first line that isn't whole or whose
/// data isn't in the file: what was written before that line is all there, so the disk is as it
/// was at one moment.
fn replay(storage: &dyn Storage, map: &Map, generation: u64) -> Replay {
    let mut replay = Replay {
        changes: 0,
        pages: BTreeSet::new(),
        superseded: Vec::new(),
    };
    let mut base = generation;
    let mut position = 0;
    let mut data = vec![0; CLUSTER];
    while position + RECORD <= JOURNAL_BYTES {
        let mut bytes = [0; RECORD];
        if !matches!(
            storage.read_at(&mut bytes, journal_offset(base) + position as u64),
            Ok(RECORD)
        ) {
            break;
        }
        let Some(record) = Record::decode(&bytes) else {
            break;
        };
        if record.base != base || record.sequence as usize != position / RECORD {
            break;
        }
        let entry = record.entry;
        if entry.kind() == kind::END_OF_EPOCH {
            // Only one commit can have been under way.
            if base != generation {
                break;
            }
            base += 1;
            position = 0;
            continue;
        }
        let cluster = record.cluster as usize;
        if cluster >= map.clusters {
            break;
        }
        match entry.kind() {
            kind::PARENT | kind::ZEROS => {}
            kind::RAW | kind::ZSTD => {
                let length = entry.length();
                let valid = entry.block as usize >= FIRST_FREE_BLOCK
                    && length > 0
                    && length <= CLUSTER
                    && (entry.kind() != kind::RAW || length == CLUSTER)
                    && matches!(storage.read_at(&mut data[..length], entry.offset()), Ok(read) if read == length)
                    && checksum(&data[..length]) == entry.checksum;
                if !valid {
                    break;
                }
            }
            _ => break,
        }
        let old = map.get(cluster);
        if old.is_stored() && old.block as usize >= FIRST_FREE_BLOCK && old.length() <= CLUSTER {
            replay.superseded.push(old.run());
        }
        map.set(cluster, entry);
        replay.changes += 1;
        replay.pages.insert(cluster / ENTRIES_PER_PAGE);
        position += RECORD;
    }
    replay
}

/// Works out which blocks are in use, from the map and what a replay still needs.
fn build_allocator(
    storage: &dyn Storage,
    map: &Map,
    table: &[(u32, u32)],
    table_extent: Run,
    superseded: &[Run],
    read_only: bool,
) -> io::Result<Allocator> {
    let mut allocator = Allocator::new(FIRST_FREE_BLOCK);
    allocator.mark_used(table_extent.block, table_extent.count);
    for &(block, _) in table {
        if block != 0 {
            allocator.mark_used(block as usize, 1);
        }
    }
    for cluster in 0..map.clusters {
        let entry = map.get(cluster);
        match entry.kind() {
            kind::PARENT | kind::ZEROS => {}
            kind::RAW | kind::ZSTD => {
                let length = entry.length();
                if (entry.block as usize) < FIRST_FREE_BLOCK
                    || length == 0
                    || length > CLUSTER
                    || entry.kind() == kind::RAW && length != CLUSTER
                {
                    return Err(corrupted("its map points outside of it"));
                }
                allocator.mark_used(entry.block as usize, entry.block_count());
            }
            _ => {
                return Err(corrupted(
                    "a cluster is stored in a way this version doesn't know",
                ));
            }
        }
    }
    for run in superseded {
        allocator.mark_used(run.block, run.count);
    }
    if !read_only {
        // What lies past the last block in use is left from writes that never made it into the map.
        let file_blocks = storage.length()?.div_ceil(BLOCK as u64) as usize;
        if file_blocks > allocator.end {
            storage.set_len((allocator.end * BLOCK) as u64)?;
        }
        // Nothing remembers across a close which free blocks still hold storage.
        allocator.mark_all_free_stale();
    }
    Ok(allocator)
}

impl Layer {
    /// Creates an empty layer of `capacity` bytes at `path`, on top of the layer whose hash is
    /// `parent`. It takes up next to nothing until it is written to.
    pub fn create(path: &Path, capacity: u64, parent: Option<[u8; 32]>) -> io::Result<()> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let result = format(&FileStorage(file), capacity, parent);
        if result.is_err() {
            _ = std::fs::remove_file(path);
        }
        result
    }

    /// Opens the layer at `path`, bringing back what its journal holds if it wasn't closed.
    /// `parent` is the layer it is built on, open for reading.
    pub fn open(path: &Path, options: &Options, parent: Option<Arc<Layer>>) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(!options.read_only)
            .open(path)?;
        Self::open_storage(Box::new(FileStorage(file)), options, parent)
    }

    pub fn open_storage(
        storage: Box<dyn Storage>,
        options: &Options,
        parent: Option<Arc<Layer>>,
    ) -> io::Result<Self> {
        Self::open_with(storage, options, parent, None)
    }

    /// `fault` is where the commit that begins a time of writing is made to fail.
    pub(crate) fn open_with(
        storage: Box<dyn Storage>,
        options: &Options,
        parent: Option<Arc<Layer>>,
        fault: Option<CommitStep>,
    ) -> io::Result<Self> {
        let superblock = Superblock::newest(&*storage)?;
        let header = Header::from(superblock);
        if header.sealed && !options.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "a sealed layer can only be read",
            ));
        }
        match (&parent, header.parent) {
            (None, Some(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the layer is built on another, which wasn't given",
                ));
            }
            (Some(_), None) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the layer isn't built on another",
                ));
            }
            (Some(parent), Some(_)) if !parent.is_read_only() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a layer can only be built on one that is only read",
                ));
            }
            _ => {}
        }
        if superblock.pages as usize != page_count(superblock.capacity) {
            return Err(corrupted("its map doesn't fit its size"));
        }
        let table = superblock.read_table(&*storage)?;
        let map = Map::new(superblock.capacity);
        let mut page = vec![0; BLOCK];
        for (index, &(block, sum)) in table.iter().enumerate() {
            if block != 0 {
                read_exact(&*storage, &mut page, block as u64 * BLOCK as u64)?;
                if checksum(&page) != sum {
                    return Err(corrupted("a page of its map doesn't match its checksum"));
                }
                map.load_page(index, &page);
            }
        }
        // The header that was just read may have been written by a commit that never got to wait
        // for it. It is the one everything goes by from here on, so it has to be on the disk before
        // a block that an older one still needs is used for something else.
        if !options.read_only {
            storage.sync()?;
        }
        let replay = replay(&*storage, &map, superblock.generation);
        let table_extent = Run {
            block: superblock.table_block as usize,
            count: superblock.table_blocks as usize,
        };
        let allocator = build_allocator(
            &*storage,
            &map,
            &table,
            table_extent,
            &replay.superseded,
            options.read_only,
        )?;

        let inner = Arc::new(Inner {
            parent_clusters: parent
                .as_ref()
                .map_or(0, |parent| cluster_count(parent.capacity())),
            parent,
            parent_hash: header.parent,
            storage,
            read_only: options.read_only,
            level: options.level,
            closed: AtomicBool::new(false),
            map: RwLock::new(map),
            stripes: (0..STRIPES).map(|_| Mutex::new(())).collect(),
            commit_lock: RwLock::new(()),
            allocator: Mutex::new(allocator),
            journal: Mutex::new(Journal::new(superblock.generation)),
            journal_written: Condvar::new(),
            scratch: Mutex::new(Vec::new()),
            commits: Mutex::new(Commits {
                generation: superblock.generation,
                table,
                table_extent,
                pending: None,
                flags: superblock.flags,
                fault,
            }),
            wake: Mutex::new(Wake {
                asked: false,
                stopping: false,
            }),
            woken: Condvar::new(),
            journal_fault: AtomicBool::new(false),
            replayed: replay.changes,
        });
        if !options.read_only {
            let mut commits = inner.commits.lock().unwrap();
            inner.fold_journal(&mut commits, replay.superseded, replay.pages)?;
            commits.fault = None;
        }
        let committer = match options.commit_interval {
            Some(interval) if !options.read_only => {
                let inner = inner.clone();
                Some(
                    std::thread::Builder::new()
                        .name("layer-commit".into())
                        .spawn(move || inner.commit_regularly(interval))?,
                )
            }
            _ => None,
        };
        Ok(Self {
            inner,
            committer: Mutex::new(committer),
        })
    }

    /// How many changes to the map were taken from the journal when the layer was opened.
    pub fn replayed_changes(&self) -> usize {
        self.inner.replayed
    }

    pub fn header(&self) -> Header {
        Header {
            capacity: self.capacity(),
            parent: self.inner.parent_hash,
            sealed: self.inner.commits.lock().unwrap().flags & SEALED != 0,
        }
    }

    /// The layer it is built on.
    pub fn parent(&self) -> Option<&Arc<Layer>> {
        self.inner.parent.as_ref()
    }

    fn stop_committer(&self) {
        let Some(thread) = self.committer.lock().unwrap().take() else {
            return;
        };
        self.inner.wake.lock().unwrap().stopping = true;
        self.inner.woken.notify_all();
        _ = thread.join();
    }

    /// Commits what is outstanding, gives freed space back and closes the file. A layer that is
    /// only read is just closed.
    pub fn close(&self) -> io::Result<()> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.stop_committer();
        let mut result = Ok(());
        if !self.inner.read_only {
            let mut commits = self.inner.commits.lock().unwrap();
            result = self.inner.commit(&mut commits, false).map(|_| ());
            if result.is_ok() {
                // Not everything at any price: someone waits for this. What is left is looked at
                // again the next time the layer is open and quiet.
                self.inner
                    .reclaim(1 << 15, 1, Some(Instant::now() + Duration::from_millis(50)));
            }
        }
        self.inner.closed.store(true, Ordering::SeqCst);
        result
    }

    /// Makes the layer read-only for good, and closes it. Layers can then be built on it.
    pub fn seal(&self) -> io::Result<()> {
        self.inner.check_writable()?;
        self.stop_committer();
        {
            let mut commits = self.inner.commits.lock().unwrap();
            commits.flags |= SEALED;
            self.inner.commit(&mut commits, true)?;
        }
        self.close()
    }

    /// Makes the disk larger. Nothing may be reading or writing it meanwhile.
    pub fn grow(&self, capacity: u64) -> io::Result<()> {
        self.inner.check_writable()?;
        let mut commits = self.inner.commits.lock().unwrap();
        {
            let mut map = self.inner.map.write().unwrap();
            if capacity < map.capacity {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a disk can't be made smaller",
                ));
            }
            map.grow(capacity);
            let pages = map.pages;
            commits.table.resize(pages, (0, 0));
        }
        self.inner.commit(&mut commits, true).map(|_| ())
    }

    pub fn stats(&self) -> Stats {
        let map = self.inner.map.read().unwrap();
        let mut stats = Stats {
            capacity: map.capacity,
            clusters: map.clusters as u64,
            file_bytes: self.inner.storage.allocated(),
            ..Stats::default()
        };
        for cluster in 0..map.clusters {
            let entry = map.get(cluster);
            match entry.kind() {
                kind::PARENT => stats.inherited += 1,
                kind::ZEROS => stats.zeros += 1,
                _ => {
                    stats.stored += 1;
                    stats.stored_bytes += entry.length() as u64;
                }
            }
        }
        stats
    }

    /// Reads every stored cluster back and says what is wrong: nothing, for a layer that is whole.
    pub fn verify(&self) -> io::Result<Vec<String>> {
        let map = self.inner.map.read().unwrap();
        let storage = &*self.inner.storage;
        let length = storage.length()?;
        let mut problems = Vec::new();
        let mut taken = vec![0u64; (length as usize / BLOCK).div_ceil(64) + 1];
        let mut data = vec![0; CLUSTER];
        let mut plain = vec![0; CLUSTER];
        let mut decompressor = zstd::bulk::Decompressor::new()?;
        for cluster in 0..map.clusters {
            let entry = map.get(cluster);
            if !entry.is_stored() {
                continue;
            }
            let mut say = |what: &str| problems.push(format!("cluster {cluster} {what}"));
            if entry.offset() + entry.length() as u64 > length {
                say("lies past the end of the file");
                continue;
            }
            let run = entry.run();
            if (run.block..run.block + run.count).any(|block| taken[block >> 6] & 1 << (block & 63) != 0) {
                say("shares its place in the file with another");
            }
            for block in run.block..run.block + run.count {
                taken[block >> 6] |= 1 << (block & 63);
            }
            let stored = &mut data[..entry.length()];
            read_exact(storage, stored, entry.offset())?;
            if checksum(stored) != entry.checksum {
                say("doesn't match its checksum");
                continue;
            }
            if entry.kind() == kind::ZSTD
                && !matches!(
                    decompressor.decompress_to_buffer(&*stored, &mut plain[..]),
                    Ok(CLUSTER)
                )
            {
                say("can't be unpacked");
            }
        }
        Ok(problems)
    }

    #[cfg(test)]
    pub(crate) fn set_commit_fault(&self, step: Option<CommitStep>) {
        self.inner.commits.lock().unwrap().fault = step;
    }

    #[cfg(test)]
    pub(crate) fn set_journal_fault(&self, fails: bool) {
        self.inner.journal_fault.store(fails, Ordering::SeqCst);
    }

    /// Where a cluster's data is in the file, for tests that damage it.
    #[cfg(test)]
    pub(crate) fn placement(&self, cluster: usize) -> Option<(u64, usize)> {
        let entry = self.inner.map.read().unwrap().get(cluster);
        entry.is_stored().then(|| (entry.offset(), entry.length()))
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        self.stop_committer();
    }
}

impl Disk for Layer {
    fn capacity(&self) -> u64 {
        self.inner.map.read().unwrap().capacity
    }

    fn is_read_only(&self) -> bool {
        self.inner.read_only
    }

    fn read(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        self.inner.read(offset, buffer)
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.inner.change(offset, data.len() as u64, Some(data))
    }

    /// Commits now, and returns once the commit is safely in the file.
    fn flush(&self) -> io::Result<()> {
        self.inner.check_writable()?;
        let mut commits = self.inner.commits.lock().unwrap();
        self.inner.commit(&mut commits, false).map(|_| ())
    }

    /// Whole clusters stop taking up room.
    fn zero(&self, offset: u64, length: u64) -> io::Result<()> {
        self.inner.change(offset, length, None)
    }

    /// Only whole clusters are let go of: a guest is promised nothing about what it reads from a
    /// part it has discarded.
    fn discard(&self, offset: u64, length: u64) -> io::Result<()> {
        self.inner
            .check_range(&self.inner.map.read().unwrap(), offset, length)?;
        let size = CLUSTER as u64;
        let first = offset.div_ceil(size);
        let end = (offset + length) / size;
        if end <= first {
            return Ok(());
        }
        self.inner.change(first * size, (end - first) * size, None)
    }
}

impl Inner {
    fn check_open(&self) -> io::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(io::Error::other("the layer is closed"));
        }
        Ok(())
    }

    fn check_range(&self, map: &Map, offset: u64, length: u64) -> io::Result<()> {
        self.check_open()?;
        if offset > map.capacity || length > map.capacity - offset {
            return Err(os(libc::EINVAL));
        }
        Ok(())
    }

    fn check_writable(&self) -> io::Result<()> {
        self.check_open()?;
        if self.read_only {
            return Err(os(libc::EROFS));
        }
        Ok(())
    }

    fn stripe(&self, cluster: usize) -> MutexGuard<'_, ()> {
        self.stripes[cluster % STRIPES].lock().unwrap()
    }

    fn take_scratch(&self) -> Scratch {
        if let Some(scratch) = self.scratch.lock().unwrap().pop() {
            return scratch;
        }
        Scratch {
            plain: vec![0; CLUSTER],
            packed: vec![0; CLUSTER],
            compressor: zstd::bulk::Compressor::new(self.level).expect("zstd has memory for a compressor"),
            decompressor: zstd::bulk::Decompressor::new().expect("zstd has memory for a decompressor"),
        }
    }

    fn give_back(&self, scratch: Scratch) {
        self.scratch.lock().unwrap().push(scratch);
    }

    /// The entry that makes a cluster read zeros.
    fn zeros(&self, cluster: usize) -> Entry {
        // Where the layer underneath has nothing, it doesn't have to be covered.
        let kind = if cluster < self.parent_clusters {
            kind::ZEROS
        } else {
            kind::PARENT
        };
        Entry::new(0, 0, kind, 0)
    }

    // Reading

    fn read(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        let map = self.map.read().unwrap();
        self.check_range(&map, offset, buffer.len() as u64)?;
        let mut scratch = self.take_scratch();
        let result = self.read_pieces(&map, offset, buffer, &mut scratch);
        self.give_back(scratch);
        result
    }

    fn read_pieces(
        &self,
        map: &Map,
        offset: u64,
        buffer: &mut [u8],
        scratch: &mut Scratch,
    ) -> io::Result<()> {
        let mut done = 0;
        while done < buffer.len() {
            let position = offset + done as u64;
            let cluster = (position / CLUSTER as u64) as usize;
            let within = (position % CLUSTER as u64) as usize;
            let count = (CLUSTER - within).min(buffer.len() - done);
            let target = &mut buffer[done..done + count];
            let _stripe = self.stripe(cluster);
            let entry = map.get(cluster);
            match entry.kind() {
                kind::PARENT => self.read_parent(position, target)?,
                kind::ZEROS => target.fill(0),
                kind::RAW => read_exact(&*self.storage, target, entry.offset() + within as u64)?,
                _ => {
                    self.unpack(entry, scratch)?;
                    target.copy_from_slice(&scratch.plain[within..within + count]);
                }
            }
            done += count;
        }
        Ok(())
    }

    /// Reads what the layer underneath holds at `position`: zeros past its end, or without one.
    fn read_parent(&self, position: u64, target: &mut [u8]) -> io::Result<()> {
        let inside = match &self.parent {
            Some(parent) => {
                let inside = parent
                    .capacity()
                    .saturating_sub(position)
                    .min(target.len() as u64) as usize;
                if inside > 0 {
                    parent.inner.read(position, &mut target[..inside])?;
                }
                inside
            }
            None => 0,
        };
        target[inside..].fill(0);
        Ok(())
    }

    /// Reads a compressed cluster into `scratch.plain`.
    fn unpack(&self, entry: Entry, scratch: &mut Scratch) -> io::Result<()> {
        if entry.kind() != kind::ZSTD {
            return Err(corrupted(
                "a cluster is stored in a way this version doesn't know",
            ));
        }
        let packed = &mut scratch.packed[..entry.length()];
        read_exact(&*self.storage, packed, entry.offset())?;
        match scratch
            .decompressor
            .decompress_to_buffer(&*packed, &mut scratch.plain[..])
        {
            Ok(CLUSTER) => Ok(()),
            _ => Err(corrupted("a cluster doesn't unpack to its size")),
        }
    }

    /// Puts a whole cluster into `scratch.plain`.
    fn load(&self, cluster: usize, entry: Entry, scratch: &mut Scratch) -> io::Result<()> {
        match entry.kind() {
            kind::PARENT => self.read_parent((cluster * CLUSTER) as u64, &mut scratch.plain),
            kind::ZEROS => {
                scratch.plain.fill(0);
                Ok(())
            }
            kind::RAW => read_exact(&*self.storage, &mut scratch.plain, entry.offset()),
            _ => self.unpack(entry, scratch),
        }
    }

    // Changing

    /// Changes a part of the disk to `data`, or to zeros without it.
    fn change(&self, offset: u64, length: u64, data: Option<&[u8]>) -> io::Result<()> {
        self.check_writable()?;
        self.check_range(&self.map.read().unwrap(), offset, length)?;
        if length == 0 {
            return Ok(());
        }
        let mut scratch = self.take_scratch();
        let result = self.change_pieces(offset, length, data, &mut scratch);
        self.give_back(scratch);
        result
    }

    fn change_pieces(
        &self,
        offset: u64,
        length: u64,
        data: Option<&[u8]>,
        scratch: &mut Scratch,
    ) -> io::Result<()> {
        let mut done = 0u64;
        while done < length {
            // A trim can reach over more clusters than the journal has room for past its limit.
            if done % (1024 * CLUSTER as u64) < CLUSTER as u64 {
                self.wait_for_journal();
            }
            let map = self.map.read().unwrap();
            let position = offset + done;
            let cluster = (position / CLUSTER as u64) as usize;
            let within = (position % CLUSTER as u64) as usize;
            let count = (CLUSTER as u64 - within as u64).min(length - done) as usize;
            let piece = data.map(|data| &data[done as usize..done as usize + count]);
            if count == CLUSTER {
                let entry = match piece {
                    Some(piece) => {
                        self.store(cluster, piece, &mut scratch.packed, &mut scratch.compressor)?
                    }
                    None => self.zeros(cluster),
                };
                let _stripe = self.stripe(cluster);
                self.publish(&map, cluster, entry)?;
            } else {
                // Read, change and write as one: another write to this cluster must see this
                // one's result.
                let _stripe = self.stripe(cluster);
                self.load(cluster, map.get(cluster), scratch)?;
                let target = &mut scratch.plain[within..within + count];
                match piece {
                    Some(piece) => target.copy_from_slice(piece),
                    None => target.fill(0),
                }
                let Scratch {
                    plain,
                    packed,
                    compressor,
                    ..
                } = scratch;
                let entry = self.store(cluster, plain, packed, compressor)?;
                self.publish(&map, cluster, entry)?;
            }
            done += count as u64;
        }
        Ok(())
    }

    /// Writes a cluster's contents to free space and says where they went. Nothing points there
    /// yet.
    fn store(
        &self,
        cluster: usize,
        plain: &[u8],
        packed: &mut [u8],
        compressor: &mut zstd::bulk::Compressor<'static>,
    ) -> io::Result<Entry> {
        if is_zero(plain) {
            return Ok(self.zeros(cluster));
        }
        // Compressed, a cluster has to save at least a block, or reading it back costs time for
        // nothing.
        let compressed = compressor
            .compress_to_buffer(plain, &mut packed[..CLUSTER - BLOCK])
            .ok()
            .filter(|&length| length > 0);
        let (bytes, length, kind) = match compressed {
            Some(length) => {
                let blocks = length.div_ceil(BLOCK);
                // Whole blocks, so that the file never ends inside one: such a block can't be
                // given back.
                packed[length..blocks * BLOCK].fill(0);
                (&packed[..blocks * BLOCK], length, kind::ZSTD)
            }
            None => (plain, CLUSTER, kind::RAW),
        };
        let block = self.allocator.lock().unwrap().allocate(bytes.len() / BLOCK);
        let entry = Entry::new(block, length, kind, checksum(&bytes[..length]));
        if let Err(error) = self.storage.write_all_at(bytes, entry.offset()) {
            self.allocator.lock().unwrap().release(block, entry.block_count());
            return Err(error);
        }
        Ok(entry)
    }

    /// Makes the map point to a cluster's new place, and returns once the journal's line about it
    /// is in the file. The caller holds the cluster's lock.
    fn publish(&self, map: &Map, cluster: usize, entry: Entry) -> io::Result<()> {
        let _shared = self.commit_lock.read().unwrap();
        let mut journal = self.journal.lock().unwrap();
        let old = map.get(cluster);
        // Nothing where nothing was: not worth a line in the journal.
        if !old.is_stored() && !entry.is_stored() && old.kind() == entry.kind() {
            return Ok(());
        }
        // Writes wait at the hard limit, and what is left past it holds more lines than are on
        // their way at once. A journal with a gap takes no more lines at all.
        if journal.broken_at.is_some() || journal.tail + 2 * RECORD > JOURNAL_BYTES {
            if entry.is_stored() {
                self.allocator
                    .lock()
                    .unwrap()
                    .release(entry.block as usize, entry.block_count());
            }
            return Err(os(if journal.broken_at.is_none() {
                libc::ENOSPC
            } else {
                libc::EIO
            }));
        }
        let record = Record {
            base: journal.base,
            cluster: cluster as u32,
            entry,
            sequence: (journal.tail / RECORD) as u32,
        };
        journal.waiting.extend_from_slice(&record.encode());
        journal.tail += RECORD;
        let mine = journal.tail;
        map.set(cluster, entry);
        journal.dirty_pages.insert(cluster / ENTRIES_PER_PAGE);
        if old.is_stored() {
            journal.pending_free.push(old.run());
        }

        // Whoever comes first writes every line that waits, in one go, and the others wait for
        // theirs to be in the file. With many writers at once that is one trip for all of them.
        while journal.written < mine {
            if journal.being_written {
                journal = self.journal_written.wait(journal).unwrap();
                continue;
            }
            let lines = std::mem::take(&mut journal.waiting);
            let start = journal.waiting_start;
            let offset = journal_offset(journal.base) + start as u64;
            journal.waiting_start += lines.len();
            journal.being_written = true;
            drop(journal);
            let outcome = if self.journal_fault.load(Ordering::SeqCst) {
                Err(os(libc::EIO))
            } else {
                self.storage.write_all_at(&lines, offset)
            };
            journal = self.journal.lock().unwrap();
            journal.being_written = false;
            journal.written = start + lines.len();
            self.journal_written.notify_all();
            if outcome.is_err() {
                // A commit needs no journal, and starts it anew.
                journal.broken_at = Some(journal.broken_at.map_or(start, |gap| gap.min(start)));
                self.ask_for_commit();
            }
        }
        // The map has changed and the data is in the file, but the guest is told that the write
        // failed: a program that went down now would bring the disk back without it.
        if journal.broken_at.is_some_and(|gap| mine > gap) {
            return Err(os(libc::EIO));
        }
        Ok(())
    }

    /// Holds a write back while the journal is nearly full, until a commit has started it anew.
    fn wait_for_journal(&self) {
        let tail = self.journal.lock().unwrap().tail;
        if tail > JOURNAL_HARD_LIMIT {
            let mut commits = self.commits.lock().unwrap();
            _ = self.commit(&mut commits, false);
        } else if tail > JOURNAL_SOFT_LIMIT {
            self.ask_for_commit();
        }
    }

    // Committing

    fn ask_for_commit(&self) {
        self.wake.lock().unwrap().asked = true;
        self.woken.notify_all();
    }

    /// Commits once every `interval`, or sooner when asked to.
    fn commit_regularly(&self, interval: Duration) {
        let mut wake = self.wake.lock().unwrap();
        loop {
            wake = self
                .woken
                .wait_timeout_while(wake, interval, |wake| !wake.asked && !wake.stopping)
                .unwrap()
                .0;
            if wake.stopping {
                return;
            }
            wake.asked = false;
            drop(wake);
            {
                // A commit that fails stays pending and is tried again at the next tick. A tick
                // with nothing to commit is a quiet moment, which is when small leftovers are
                // given back.
                let mut commits = self.commits.lock().unwrap();
                if matches!(self.commit(&mut commits, false), Ok(false)) {
                    self.reclaim(4096, 1, None);
                }
            }
            wake = self.wake.lock().unwrap();
        }
    }

    /// Makes the file hold the disk as it is now, and says whether there was anything to do.
    fn commit(&self, commits: &mut Commits, force: bool) -> io::Result<bool> {
        if self.read_only || self.closed.load(Ordering::SeqCst) {
            return Ok(false);
        }
        // A commit that failed earlier is owed first: what was written since comes after it.
        if commits.pending.is_some() {
            let frees = self.finish_pending(commits)?;
            self.release(&frees);
        }
        {
            let map = self.map.read().unwrap();
            // Nobody is changing the map now, so every line of this half is in the file.
            let _frozen = self.commit_lock.write().unwrap();
            let mut journal = self.journal.lock().unwrap();
            if !force && journal.tail == 0 && journal.dirty_pages.is_empty() {
                return Ok(false);
            }
            // A half with a gap is never followed to its end, and may not take another line.
            if journal.broken_at.is_none() {
                let end = Record {
                    base: journal.base,
                    cluster: 0,
                    entry: Entry::new(0, 0, kind::END_OF_EPOCH, 0),
                    sequence: (journal.tail / RECORD) as u32,
                };
                self.storage
                    .write_all_at(&end.encode(), journal_offset(journal.base) + journal.tail as u64)?;
            }
            let next = commits.generation + 1;
            commits.pending = Some(PendingCommit {
                generation: next,
                pages: journal
                    .dirty_pages
                    .iter()
                    .map(|&page| (page, map.page_bytes(page)))
                    .collect(),
                frees: std::mem::take(&mut journal.pending_free),
                capacity: map.capacity,
                page_count: map.pages,
                flags: commits.flags,
            });
            let had_gap = journal.broken_at.is_some();
            *journal = Journal::new(next);
            // Following the journal from the last commit ends at the gap, and never gets to the
            // lines of this half. It takes none until this commit is in the file.
            if had_gap {
                journal.broken_at = Some(0);
            }
        }
        let frees = self.finish_pending(commits)?;
        self.release(&frees);
        // The half of the journal that the next commit moves to holds nothing that is still needed.
        self.storage
            .punch(journal_offset(commits.generation + 1), JOURNAL_BYTES as u64);
        // Only what a discard or a large rewrite has freed. A block that a small write let go of
        // is likely to be written again in a moment, and writing into a hole costs a multiple of
        // writing over what is there.
        self.reclaim(4096, LARGE_RUN, None);
        Ok(true)
    }

    /// Writes the frozen map and the block that points to it, and returns what is free from then on.
    fn finish_pending(&self, commits: &mut Commits) -> io::Result<Vec<Run>> {
        let Some(mut commit) = commits.pending.take() else {
            return Ok(Vec::new());
        };
        match self.write_commit(commits, &mut commit) {
            Ok(()) => {
                commits.generation = commit.generation;
                // The journal leads on from this commit now, whatever came before it.
                let mut journal = self.journal.lock().unwrap();
                if journal.broken_at == Some(0) && journal.tail == 0 {
                    journal.broken_at = None;
                }
                Ok(commit.frees)
            }
            Err(error) => {
                // What a try that failed left behind is let go of with the rest.
                commits.pending = Some(commit);
                Err(error)
            }
        }
    }

    fn write_commit(&self, commits: &mut Commits, commit: &mut PendingCommit) -> io::Result<()> {
        if commits.table.len() < commit.page_count {
            commits.table.resize(commit.page_count, (0, 0));
        }
        for (index, bytes) in &commit.pages {
            let (old, _) = commits.table[*index];
            // Noted before the page is written: a try that fails here leaves the next one to free it.
            if old != 0 {
                commit.frees.push(Run {
                    block: old as usize,
                    count: 1,
                });
            }
            if is_zero(bytes) {
                commits.table[*index] = (0, 0);
            } else {
                let block = self.allocator.lock().unwrap().allocate(1);
                commits.table[*index] = (block as u32, checksum(bytes));
                self.storage.write_all_at(bytes, (block * BLOCK) as u64)?;
            }
        }
        commit.pages.clear();

        let mut encoded = vec![0; (commits.table.len() * 8).div_ceil(BLOCK) * BLOCK];
        for (index, &(block, sum)) in commits.table.iter().enumerate() {
            put_u32(&mut encoded, index * 8, block);
            put_u32(&mut encoded, index * 8 + 4, sum);
        }
        let old_table = commits.table_extent;
        let mut new_table = Run {
            block: 0,
            count: encoded.len() / BLOCK,
        };
        if new_table.count > 0 {
            new_table.block = self.allocator.lock().unwrap().allocate(new_table.count);
        }
        // Noted before it is written: a try that fails here leaves blocks behind that the next
        // try frees.
        commits.table_extent = new_table;
        if old_table.count > 0 {
            commit.frees.push(old_table);
        }
        if new_table.count > 0 {
            self.storage
                .write_all_at(&encoded, (new_table.block * BLOCK) as u64)?;
        }
        if commits.fault == Some(CommitStep::MapWritten) {
            return Err(os(libc::EIO));
        }
        // Everything the new block will point to has to reach the disk before the block does.
        // That takes a barrier, not the wait for all of it to be there: the wait comes once, at
        // the end.
        self.storage.barrier()?;
        if commits.fault == Some(CommitStep::MapSynchronized) {
            return Err(os(libc::EIO));
        }
        let superblock = Superblock {
            capacity: commit.capacity,
            generation: commit.generation,
            table_block: new_table.block as u32,
            table_blocks: new_table.count as u32,
            table_checksum: checksum(&encoded),
            pages: commit.page_count as u32,
            flags: commit.flags,
            parent: self.parent_hash.unwrap_or_default(),
        };
        // The two blocks take turns, so the one being written is never the one a crash falls
        // back on.
        self.storage
            .write_all_at(&superblock.encode(), (commit.generation % 2) * BLOCK as u64)?;
        if commits.fault == Some(CommitStep::SuperblockWritten) {
            return Err(os(libc::EIO));
        }
        self.storage.sync()
    }

    /// Begins a time of writing with a commit of the layer as it was opened, with what the
    /// journal brought back.
    ///
    /// The commit skips two numbers. A line of the journal is told from an older one by the
    /// number of the commit it comes after, and lines of the last commit and of one that never
    /// finished can be in the journal still: after a crash of the host also behind lines that are
    /// missing, where nothing finds them now. Under a number none of them carries, no line
    /// written from here on can be taken to go on into them. Three ahead rather than two, so that
    /// the two blocks that point to the map still take turns.
    fn fold_journal(
        &self,
        commits: &mut Commits,
        superseded: Vec<Run>,
        pages: BTreeSet<usize>,
    ) -> io::Result<()> {
        let next = commits.generation + 3;
        {
            let map = self.map.read().unwrap();
            commits.pending = Some(PendingCommit {
                generation: next,
                pages: pages.iter().map(|&page| (page, map.page_bytes(page))).collect(),
                frees: superseded,
                capacity: map.capacity,
                page_count: map.pages,
                flags: commits.flags,
            });
        }
        let frees = self.finish_pending(commits)?;
        *self.journal.lock().unwrap() = Journal::new(next);
        // Tidying only: nothing in the journal is needed any more.
        self.storage.punch(journal_offset(0), 2 * JOURNAL_BYTES as u64);
        self.release(&frees);
        Ok(())
    }

    fn release(&self, runs: &[Run]) {
        if runs.is_empty() {
            return;
        }
        let mut allocator = self.allocator.lock().unwrap();
        for run in runs {
            allocator.release(run.block, run.count);
        }
    }

    /// Gives the host back the storage of blocks that nothing uses any more: of at most `limit`
    /// runs of at least `smallest` blocks, until `deadline`.
    fn reclaim(&self, limit: usize, smallest: usize, deadline: Option<Instant>) {
        // Free blocks at the end are cut off, not punched: a hole there wouldn't last. With the
        // lock held, so that nobody is handed a block behind the new end before the file is that
        // short.
        {
            let mut allocator = self.allocator.lock().unwrap();
            if let Some(end) = allocator.trim_end() {
                _ = self.storage.set_len((end * BLOCK) as u64);
            }
        }
        let Ok(length) = self.storage.length() else { return };
        // Blocks past the end of the file were handed out and never written. They hold nothing.
        let file_blocks = (length / BLOCK as u64) as usize;
        let mut position = FIRST_FREE_BLOCK;
        let mut done = 0;
        while done < limit && deadline.is_none_or(|deadline| Instant::now() < deadline) {
            let Some(run) = self.allocator.lock().unwrap().take_stale_run(position, smallest) else {
                break;
            };
            let count = run.count.min(file_blocks.saturating_sub(run.block));
            if count > 0 {
                self.storage
                    .punch((run.block * BLOCK) as u64, (count * BLOCK) as u64);
            }
            self.allocator.lock().unwrap().finish_reclaiming(run);
            position = run.block + run.count;
            done += 1;
        }
    }
}

/// The SHA-256 of a file: what a sealed layer is named by.
pub fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut context = graviola::hashing::sha2::Sha256Context::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        match file.read(&mut buffer)? {
            0 => return Ok(context.finish()),
            count => context.update(&buffer[..count]),
        }
    }
}

pub fn to_hex(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn from_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut hash = [0; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(hash)
}
