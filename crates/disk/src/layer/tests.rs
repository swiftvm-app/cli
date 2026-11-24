use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::*;

/// Numbers that look random, the same each run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit.max(1) as u64) as usize
    }

    /// Bytes that don't compress.
    fn noise(&mut self, count: usize) -> Vec<u8> {
        (0..count).map(|_| self.next() as u8).collect()
    }
}

/// Bytes that compress well, and differ with `tag`.
fn text(count: usize, tag: usize) -> Vec<u8> {
    let line = format!("cluster {tag} holds this line over and over, as files do. ").into_bytes();
    (0..count).map(|index| line[index % line.len()]).collect()
}

/// A layer in a folder of its own, next to what it should hold.
struct Fixture {
    folder: PathBuf,
    path: PathBuf,
    expected: Vec<u8>,
    random: Random,
}

impl Fixture {
    fn new(test: &str, capacity: usize) -> Self {
        let folder = std::env::temp_dir().join(format!("layer-{test}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("disk.layer");
        Layer::create(&path, capacity as u64, None).unwrap();
        Self {
            folder,
            path,
            expected: vec![0; capacity],
            random: Random(0x9E37_79B9_7F4A_7C15),
        }
    }

    fn open(&self) -> Layer {
        Layer::open(&self.path, &quiet(), None).unwrap()
    }

    fn open_failing(&self, step: CommitStep) -> io::Result<Layer> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .unwrap();
        Layer::open_with(Box::new(FileStorage(file)), &quiet(), None, Some(step))
    }

    fn write(&mut self, bytes: &[u8], offset: usize, layer: &Layer) {
        layer.write(offset as u64, bytes).unwrap();
        self.expected[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    fn noise(&mut self, count: usize) -> Vec<u8> {
        self.random.noise(count)
    }

    /// What the journal's part of the file holds.
    fn journal(&self) -> Vec<u8> {
        let file = std::fs::File::open(&self.path).unwrap();
        let mut bytes = vec![0; (GUARD_BLOCK - 2) * BLOCK];
        file.read_exact_at(&mut bytes, (2 * BLOCK) as u64).unwrap();
        bytes
    }

    /// Puts back what the journal held at an earlier time, lines that were tidied away since
    /// included.
    fn put_back(&self, journal: &[u8]) {
        let file = OpenOptions::new().write(true).open(&self.path).unwrap();
        file.write_all_at(journal, (2 * BLOCK) as u64).unwrap();
    }

    fn allocated(&self) -> usize {
        std::fs::metadata(&self.path).unwrap().blocks() as usize * 512
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.folder);
    }
}

use std::os::unix::fs::{FileExt, MetadataExt};

/// Commits only when asked, so that a test says when.
fn quiet() -> Options {
    Options {
        commit_interval: None,
        ..Options::default()
    }
}

fn contents(layer: &Layer) -> Vec<u8> {
    let mut bytes = vec![0xAA; layer.capacity() as usize];
    layer.read(0, &mut bytes).unwrap();
    bytes
}

#[test]
fn keeps_what_is_written_and_grows() {
    let mut fixture = Fixture::new("grows", 8 * CLUSTER);
    let mut layer = fixture.open();
    fixture.write(&text(3 * CLUSTER, 1), 0, &layer);
    let noise = fixture.noise(CLUSTER);
    fixture.write(&noise, 3 * CLUSTER, &layer);
    // Across two clusters, neither of them whole, and on no block's edge.
    let noise = fixture.noise(5000);
    fixture.write(&noise, CLUSTER - 700, &layer);
    fixture.write(&text(999, 2), 6 * CLUSTER + 13, &layer);
    fixture.write(&vec![0; CLUSTER], 2 * CLUSTER, &layer);
    assert!(contents(&layer) == fixture.expected);
    layer.close().unwrap();

    layer = fixture.open();
    assert_eq!(layer.replayed_changes(), 0);
    assert!(contents(&layer) == fixture.expected);

    // Far enough to need more pages of the map.
    let grown = 1200 * CLUSTER + 512;
    layer.grow(grown as u64).unwrap();
    fixture.expected.resize(grown, 0);
    fixture.write(&text(3000, 3), grown - 3000, &layer);
    layer.close().unwrap();

    assert_eq!(Header::read(&fixture.path).unwrap().capacity, grown as u64);
    layer = fixture.open();
    assert!(contents(&layer) == fixture.expected);
    assert!(layer.verify().unwrap().is_empty());
    layer.close().unwrap();
}

#[test]
fn loses_nothing_when_the_program_goes_down() {
    let mut fixture = Fixture::new("down", 16 * CLUSTER);
    let mut layer = fixture.open();
    fixture.write(&text(4 * CLUSTER, 1), 0, &layer);
    layer.flush().unwrap();
    let noise = fixture.noise(2 * CLUSTER);
    fixture.write(&noise, CLUSTER, &layer);
    fixture.write(&text(100, 2), 9 * CLUSTER + 5, &layer);
    layer.discard(0, CLUSTER as u64).unwrap();
    fixture.expected[..CLUSTER].fill(0);
    drop(layer);

    layer = fixture.open();
    assert_eq!(layer.replayed_changes(), 4);
    assert!(contents(&layer) == fixture.expected);

    // Once more, over the same clusters: what the first time left in the journal mustn't come back.
    fixture.write(&text(3 * CLUSTER, 3), 0, &layer);
    let noise = fixture.noise(700);
    fixture.write(&noise, 9 * CLUSTER, &layer);
    drop(layer);

    layer = fixture.open();
    assert!(contents(&layer) == fixture.expected);
    layer.close().unwrap();
    layer = fixture.open();
    assert_eq!(layer.replayed_changes(), 0);
    assert!(contents(&layer) == fixture.expected);
    layer.close().unwrap();
}

/// When the host goes down, data that was on its way may not be in the file while the journal's
/// line about it is. The disk then has to be as it was just before that line, and no later.
#[test]
fn comes_back_as_it_was_before_data_that_never_arrived() {
    let mut fixture = Fixture::new("arrived", 8 * CLUSTER);
    let mut layer = fixture.open();
    fixture.write(&text(4 * CLUSTER, 1), 0, &layer);
    layer.flush().unwrap();

    let noise = fixture.noise(CLUSTER);
    fixture.write(&noise, 0, &layer);
    let before = fixture.expected.clone();
    let noise = fixture.noise(CLUSTER);
    fixture.write(&noise, CLUSTER, &layer);
    let noise = fixture.noise(CLUSTER);
    fixture.write(&noise, 2 * CLUSTER, &layer);
    let (offset, length) = layer.placement(1).unwrap();
    drop(layer);

    let file = OpenOptions::new().write(true).open(&fixture.path).unwrap();
    file.write_all_at(&vec![0; length], offset).unwrap();

    layer = fixture.open();
    assert_eq!(layer.replayed_changes(), 1);
    assert!(contents(&layer) == before);
    layer.close().unwrap();
}

/// A line of the journal that was only half written ends what is followed.
#[test]
fn a_torn_line_of_the_journal_ends_the_replay() {
    let mut fixture = Fixture::new("torn", 8 * CLUSTER);
    let mut layer = fixture.open();
    fixture.write(&text(CLUSTER, 1), 0, &layer);
    let before = fixture.expected.clone();
    fixture.write(&text(CLUSTER, 2), CLUSTER, &layer);
    fixture.write(&text(CLUSTER, 3), 2 * CLUSTER, &layer);
    drop(layer);

    // The second line of the half that the open after the last commit writes to.
    let generation = Superblock::newest(&FileStorage(std::fs::File::open(&fixture.path).unwrap()))
        .unwrap()
        .generation;
    let line = journal_offset(generation) + RECORD as u64;
    let file = OpenOptions::new().write(true).open(&fixture.path).unwrap();
    file.write_all_at(&[0; RECORD / 2], line + (RECORD / 2) as u64)
        .unwrap();

    layer = fixture.open();
    assert_eq!(layer.replayed_changes(), 1);
    assert!(contents(&layer) == before);
    layer.close().unwrap();
}

#[test]
fn survives_a_commit_that_fails() {
    for step in [
        CommitStep::MapWritten,
        CommitStep::MapSynchronized,
        CommitStep::SuperblockWritten,
    ] {
        let mut fixture = Fixture::new(&format!("commit-{step:?}"), 8 * CLUSTER);
        let mut layer = fixture.open();
        fixture.write(&text(2 * CLUSTER, 1), 0, &layer);
        layer.flush().unwrap();
        let noise = fixture.noise(CLUSTER + 10);
        fixture.write(&noise, CLUSTER, &layer);

        layer.set_commit_fault(Some(step));
        assert!(layer.flush().is_err());
        // The guest carries on while the commit is still owed.
        fixture.write(&text(300, 2), 5 * CLUSTER, &layer);
        assert!(contents(&layer) == fixture.expected);
        drop(layer);

        layer = fixture.open();
        assert!(contents(&layer) == fixture.expected, "{step:?}");

        // And a commit that fails and then gets through leaves a layer that closes and opens as
        // usual.
        let noise = fixture.noise(CLUSTER);
        fixture.write(&noise, 0, &layer);
        layer.set_commit_fault(Some(step));
        assert!(layer.flush().is_err());
        layer.set_commit_fault(None);
        fixture.write(&text(CLUSTER, 3), 3 * CLUSTER, &layer);
        layer.close().unwrap();
        layer = fixture.open();
        assert_eq!(layer.replayed_changes(), 0);
        assert!(contents(&layer) == fixture.expected, "{step:?}");
        assert!(layer.verify().unwrap().is_empty());
        layer.close().unwrap();
    }
}

/// The open after a crash writes to the file itself. Cut short, it has to leave everything in
/// place that the next open goes by, and what it has brought back must never be followed again.
#[test]
fn an_open_that_is_cut_short_loses_nothing() {
    for step in [
        CommitStep::MapWritten,
        CommitStep::MapSynchronized,
        CommitStep::SuperblockWritten,
    ] {
        let mut fixture = Fixture::new(&format!("open-{step:?}"), 8 * CLUSTER);
        let mut layer = fixture.open();
        let noise = fixture.noise(2 * CLUSTER);
        fixture.write(&noise, 0, &layer);
        layer.flush().unwrap();
        // Changes that replace what the last commit holds, and each other.
        let noise = fixture.noise(CLUSTER);
        fixture.write(&noise, CLUSTER, &layer);
        let noise = fixture.noise(CLUSTER);
        fixture.write(&noise, CLUSTER, &layer);
        layer.discard(0, CLUSTER as u64).unwrap();
        fixture.expected[..CLUSTER].fill(0);
        // And a commit that is still owed when the program goes down, with changes after it.
        layer.set_commit_fault(Some(CommitStep::MapWritten));
        assert!(layer.flush().is_err());
        let noise = fixture.noise(CLUSTER);
        fixture.write(&noise, CLUSTER, &layer);
        fixture.write(&text(500, 1), 3 * CLUSTER, &layer);
        drop(layer);
        let journal = fixture.journal();

        assert!(fixture.open_failing(step).is_err());
        layer = fixture.open();
        assert!(contents(&layer) == fixture.expected, "{step:?}");
        drop(layer);

        // As if the journal had never been tidied after that.
        fixture.put_back(&journal);
        layer = fixture.open();
        assert_eq!(layer.replayed_changes(), 0);
        assert!(contents(&layer) == fixture.expected, "{step:?}");
        let noise = fixture.noise(CLUSTER);
        fixture.write(&noise, CLUSTER, &layer);
        layer.close().unwrap();
        layer = fixture.open();
        assert!(contents(&layer) == fixture.expected, "{step:?}");
        layer.close().unwrap();
    }
}

#[test]
fn calls_no_write_done_that_the_journal_does_not_cover() {
    let mut fixture = Fixture::new("uncovered", 64 * CLUSTER);
    let mut layer = Arc::new(fixture.open());
    fixture.write(&text(CLUSTER, 1), 0, &layer);

    layer.set_journal_fault(true);
    let noise = fixture.noise(CLUSTER);
    // Many at once, so that one writes the lines of the others: none of them may be told it is done.
    let done: Vec<bool> = std::thread::scope(|scope| {
        let writers: Vec<_> = (0..32)
            .map(|writer| {
                let (layer, noise) = (&layer, &noise);
                scope.spawn(move || layer.write(((writer + 1) * CLUSTER) as u64, noise).is_ok())
            })
            .collect();
        writers.into_iter().map(|writer| writer.join().unwrap()).collect()
    });
    assert!(done.iter().all(|done| !done));

    // A commit starts the journal anew, and what is on the disk then stays, the program going
    // down or not.
    layer.set_journal_fault(false);
    layer.flush().unwrap();
    fixture.expected = contents(&layer);
    fixture.write(&text(900, 2), 40 * CLUSTER, &layer);
    drop(layer);
    layer = Arc::new(fixture.open());
    assert_eq!(layer.replayed_changes(), 1);
    assert!(contents(&layer) == fixture.expected);
    layer.close().unwrap();
}

#[test]
fn takes_less_space_and_gives_it_back() {
    const MEGABYTE: usize = 1 << 20;
    let mut fixture = Fixture::new("space", 64 * MEGABYTE);
    let mut layer = fixture.open();
    fixture.write(&text(24 * MEGABYTE, 1), 0, &layer);
    let noise = fixture.noise(8 * MEGABYTE);
    fixture.write(&noise, 32 * MEGABYTE, &layer);
    layer.close().unwrap();
    // The text shrinks to a fraction, the noise stays as large as it is.
    assert!(fixture.allocated() > 8 * MEGABYTE);
    assert!(fixture.allocated() < 12 * MEGABYTE, "{}", fixture.allocated());

    layer = fixture.open();
    // Writing the same place again and again must not make the file grow.
    for _ in 0..4 {
        let noise = fixture.noise(8 * MEGABYTE);
        fixture.write(&noise, 32 * MEGABYTE, &layer);
        layer.flush().unwrap();
    }
    layer.discard(MEGABYTE as u64, 60 * MEGABYTE as u64).unwrap();
    layer.close().unwrap();
    assert!(fixture.allocated() < 2 * MEGABYTE, "{}", fixture.allocated());

    layer = fixture.open();
    fixture.expected[MEGABYTE..61 * MEGABYTE].fill(0);
    assert!(contents(&layer) == fixture.expected);
    layer.close().unwrap();
}

#[test]
fn serves_many_writers_at_once() {
    let fixture = Fixture::new("writers", 256 * CLUSTER);
    let layer = Layer::open(
        &fixture.path,
        &Options {
            commit_interval: Some(Duration::from_millis(10)),
            ..Options::default()
        },
        None,
    )
    .unwrap();
    const WORKERS: usize = 8;
    let region = fixture.expected.len() / WORKERS;
    // Each worker has a part of the disk to itself and checks it as it goes; all of them share
    // the map, the journal and the commits that run meanwhile.
    let good: Vec<bool> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WORKERS)
            .map(|worker| {
                let layer = &layer;
                scope.spawn(move || {
                    let part = text(region, worker);
                    let mut mine = vec![0; region];
                    let mut random = Random(worker as u64 * 0x9E37_79B9 + 12345);
                    for round in 0..300 {
                        let length = 1 + random.below(3 * CLUSTER);
                        let offset = random.below(region - length);
                        let bytes: Vec<u8> = part[offset..offset + length]
                            .iter()
                            .map(|byte| byte.wrapping_add(round as u8))
                            .collect();
                        layer.write((worker * region + offset) as u64, &bytes).unwrap();
                        mine[offset..offset + length].copy_from_slice(&bytes);
                        if round % 16 == 0 {
                            let mut read = vec![0; region];
                            layer.read((worker * region) as u64, &mut read).unwrap();
                            if read != mine {
                                return false;
                            }
                        }
                    }
                    let mut read = vec![0; region];
                    layer.read((worker * region) as u64, &mut read).unwrap();
                    read == mine
                })
            })
            .collect();
        workers.into_iter().map(|worker| worker.join().unwrap()).collect()
    });
    assert!(good.iter().all(|good| *good));
    let live = contents(&layer);
    layer.close().unwrap();

    let reopened = fixture.open();
    assert!(contents(&reopened) == live);
    assert!(reopened.verify().unwrap().is_empty());
    reopened.close().unwrap();
}

/// Writes, zeros and discards of every size, checked against a plain copy as they go and across
/// closes and crashes of the program.
#[test]
fn matches_a_plain_disk_through_random_changes() {
    let capacity = 40 * CLUSTER + 1234;
    let mut fixture = Fixture::new("random", capacity);
    let mut layer = fixture.open();
    for round in 0..600 {
        let offset = fixture.random.below(capacity);
        let length = 1 + fixture.random.below((capacity - offset).min(3 * CLUSTER + 100));
        match fixture.random.below(10) {
            0 => {
                layer.zero(offset as u64, length as u64).unwrap();
                fixture.expected[offset..offset + length].fill(0);
            }
            1 => {
                layer.discard(offset as u64, length as u64).unwrap();
                let first = offset.div_ceil(CLUSTER) * CLUSTER;
                let end = (offset + length) / CLUSTER * CLUSTER;
                if end > first {
                    fixture.expected[first..end].fill(0);
                }
            }
            2..=5 => {
                let bytes = fixture.noise(length);
                fixture.write(&bytes, offset, &layer);
            }
            _ => fixture.write(&text(length, round), offset, &layer),
        }
        let mut read = vec![0; length];
        layer.read(offset as u64, &mut read).unwrap();
        assert!(read == fixture.expected[offset..offset + length], "round {round}");
        match round % 97 {
            30 => layer.flush().unwrap(),
            60 => {
                drop(layer);
                layer = fixture.open();
                assert!(contents(&layer) == fixture.expected, "round {round}");
            }
            90 => {
                layer.close().unwrap();
                layer = fixture.open();
                assert!(contents(&layer) == fixture.expected, "round {round}");
            }
            _ => {}
        }
    }
    assert!(layer.verify().unwrap().is_empty());
    layer.close().unwrap();
}

#[test]
fn finds_a_damaged_cluster() {
    let mut fixture = Fixture::new("damaged", 4 * CLUSTER);
    let mut layer = fixture.open();
    fixture.write(&text(CLUSTER, 1), 0, &layer);
    let noise = fixture.noise(CLUSTER);
    fixture.write(&noise, CLUSTER, &layer);
    layer.close().unwrap();
    layer = Layer::open(&fixture.path, &Options::read_only(), None).unwrap();
    let (offset, _) = layer.placement(1).unwrap();
    drop(layer);

    let file = OpenOptions::new().write(true).open(&fixture.path).unwrap();
    file.write_all_at(b"damage", offset + 100).unwrap();
    layer = Layer::open(&fixture.path, &Options::read_only(), None).unwrap();
    assert_eq!(layer.verify().unwrap(), ["cluster 1 doesn't match its checksum"]);
}

/// A layer on top of another reads through to it where it has nothing of its own, and the one
/// underneath never changes.
#[test]
fn a_layer_reads_through_to_the_one_it_is_built_on() {
    let mut base = Fixture::new("base", 6 * CLUSTER + 100);
    let layer = base.open();
    base.write(&text(6 * CLUSTER + 100, 1), 0, &layer);
    let noise = base.noise(CLUSTER);
    base.write(&noise, 2 * CLUSTER, &layer);
    layer.seal().unwrap();
    let hash = hash_file(&base.path).unwrap();
    assert!(Header::read(&base.path).unwrap().sealed);
    // Sealed, it is only read.
    assert!(Layer::open(&base.path, &quiet(), None).is_err());

    let parent = Arc::new(Layer::open(&base.path, &Options::read_only(), None).unwrap());
    let path = base.folder.join("child.layer");
    // Larger than what it is built on: the rest reads zeros.
    let capacity = 10 * CLUSTER;
    Layer::create(&path, capacity as u64, Some(hash)).unwrap();
    assert!(Layer::open(&path, &quiet(), None).is_err());
    let mut child = Layer::open(&path, &quiet(), Some(parent.clone())).unwrap();
    let mut expected = base.expected.clone();
    expected.resize(capacity, 0);
    assert!(contents(&child) == expected);

    let mut change = |child: &Layer, offset: usize, bytes: &[u8]| {
        child.write(offset as u64, bytes).unwrap();
        expected[offset..offset + bytes.len()].copy_from_slice(bytes);
    };
    // Part of a cluster underneath, a whole one, one across the end of the one underneath.
    change(&child, 100, b"written over");
    change(&child, CLUSTER, &text(CLUSTER, 2));
    change(&child, 6 * CLUSTER + 50, &text(200, 3));
    // Zeros over what is underneath stay zeros, also across a close.
    child.zero((3 * CLUSTER) as u64, CLUSTER as u64).unwrap();
    expected[3 * CLUSTER..4 * CLUSTER].fill(0);
    child.discard((4 * CLUSTER) as u64, CLUSTER as u64).unwrap();
    expected[4 * CLUSTER..5 * CLUSTER].fill(0);
    child.zero((5 * CLUSTER + 10) as u64, 20).unwrap();
    expected[5 * CLUSTER + 10..5 * CLUSTER + 30].fill(0);
    assert!(contents(&child) == expected);
    let stats = child.stats();
    assert_eq!((stats.stored, stats.zeros), (4, 2));
    drop(child);

    child = Layer::open(&path, &quiet(), Some(parent.clone())).unwrap();
    assert!(contents(&child) == expected);
    child.close().unwrap();
    assert!(contents(&parent) == base.expected);
    assert_eq!(hash_file(&base.path).unwrap(), hash);
}

/// What a storage holds, a block at a time.
#[derive(Clone, Default)]
struct Blocks {
    length: u64,
    blocks: BTreeMap<u64, Vec<u8>>,
}

impl Blocks {
    fn read(&self, buffer: &mut [u8], offset: u64) -> usize {
        let end = (offset + buffer.len() as u64).min(self.length);
        let mut position = offset;
        while position < end {
            let block = position / BLOCK as u64;
            let within = (position % BLOCK as u64) as usize;
            let count = (BLOCK - within).min((end - position) as usize);
            let target = &mut buffer[(position - offset) as usize..][..count];
            match self.blocks.get(&block) {
                Some(bytes) => target.copy_from_slice(&bytes[within..within + count]),
                None => target.fill(0),
            }
            position += count as u64;
        }
        end.saturating_sub(offset) as usize
    }

    fn apply(&mut self, change: &Change) {
        match change {
            Change::Write(offset, data) => {
                let block = offset / BLOCK as u64;
                let within = (offset % BLOCK as u64) as usize;
                let bytes = self.blocks.entry(block).or_insert_with(|| vec![0; BLOCK]);
                bytes[within..within + data.len()].copy_from_slice(data);
                self.length = self.length.max(offset + data.len() as u64);
            }
            Change::Length(length) => {
                let length = *length;
                self.blocks.retain(|&block, _| block * (BLOCK as u64) < length);
                if let Some(bytes) = self.blocks.get_mut(&(length / BLOCK as u64)) {
                    bytes[(length % BLOCK as u64) as usize..].fill(0);
                }
                self.length = length;
            }
            Change::Punch(offset, length) => {
                for position in (*offset..offset + length).step_by(BLOCK) {
                    self.blocks.remove(&(position / BLOCK as u64));
                }
            }
        }
    }
}

/// A change that hasn't reached the disk yet. Writes are kept a block at a time: a crash can
/// tear a larger one.
enum Change {
    Write(u64, Vec<u8>),
    Length(u64),
    Punch(u64, u64),
}

struct MemoryState {
    /// What is on the disk for sure.
    durable: Blocks,
    /// What reads see.
    current: Blocks,
    /// Changes since the last sync, in groups that barriers keep in order.
    epochs: Vec<Vec<Change>>,
}

/// A storage in memory that a host's crash can be played on.
#[derive(Clone)]
struct Memory(Arc<Mutex<MemoryState>>);

impl Memory {
    fn new(blocks: Blocks) -> Self {
        Self(Arc::new(Mutex::new(MemoryState {
            durable: blocks.clone(),
            current: blocks,
            epochs: vec![Vec::new()],
        })))
    }

    fn change(&self, change: Change) {
        let mut state = self.0.lock().unwrap();
        state.current.apply(&change);
        state.epochs.last_mut().unwrap().push(change);
    }

    /// What the disk holds after the host went down now: every group of changes before one, any
    /// of that one's, and none after.
    fn crash(&self, random: &mut Random) -> Memory {
        let state = self.0.lock().unwrap();
        let mut blocks = state.durable.clone();
        let cut = random.below(state.epochs.len());
        for (index, epoch) in state.epochs.iter().enumerate().take(cut + 1) {
            for change in epoch {
                if index < cut || random.below(2) == 0 {
                    blocks.apply(change);
                }
            }
        }
        Memory::new(blocks)
    }
}

impl Storage for Memory {
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
        Ok(self.0.lock().unwrap().current.read(buffer, offset))
    }

    fn write_all_at(&self, data: &[u8], offset: u64) -> io::Result<()> {
        let mut done = 0;
        while done < data.len() {
            let position = offset + done as u64;
            let count = (BLOCK - (position % BLOCK as u64) as usize).min(data.len() - done);
            self.change(Change::Write(position, data[done..done + count].to_vec()));
            done += count;
        }
        Ok(())
    }

    fn length(&self) -> io::Result<u64> {
        Ok(self.0.lock().unwrap().current.length)
    }

    fn set_len(&self, length: u64) -> io::Result<()> {
        self.change(Change::Length(length));
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        let mut state = self.0.lock().unwrap();
        state.durable = state.current.clone();
        state.epochs = vec![Vec::new()];
        Ok(())
    }

    fn barrier(&self) -> io::Result<()> {
        self.0.lock().unwrap().epochs.push(Vec::new());
        Ok(())
    }

    fn punch(&self, offset: u64, length: u64) {
        // Whole blocks, as a file system gives back.
        let first = offset.div_ceil(BLOCK as u64) * BLOCK as u64;
        let end = (offset + length) / BLOCK as u64 * BLOCK as u64;
        if end > first {
            self.change(Change::Punch(first, end - first));
        }
    }

    fn allocated(&self) -> u64 {
        self.0.lock().unwrap().current.blocks.len() as u64 * BLOCK as u64
    }
}

/// After a crash of the host the disk has to be as it was at one moment since its last commit:
/// after one of the clusters that were changed since, and before the next.
#[test]
fn a_crash_of_the_host_brings_back_one_moment_since_the_last_commit() {
    let capacity = 12 * CLUSTER;
    for seed in 0..60u64 {
        let mut random = Random(seed * 7919 + 1);
        let memory = Memory::new(Blocks::default());
        format(&memory, capacity as u64, None).unwrap();
        let layer = Layer::open_storage(Box::new(memory.clone()), &quiet(), None).unwrap();
        let mut model = vec![0u8; capacity];
        // What the disk may come back as: each moment since the last commit.
        let mut moments = vec![checksum(&model)];
        for step in 0..40 {
            let offset = random.below(capacity);
            let length = 1 + random.below((capacity - offset).min(2 * CLUSTER + 300));
            let data = match random.below(6) {
                0 => None,
                1 | 2 => Some(random.noise(length)),
                _ => Some(text(length, step)),
            };
            match &data {
                Some(data) => layer.write(offset as u64, data).unwrap(),
                None => layer.zero(offset as u64, length as u64).unwrap(),
            }
            // Clusters change one after the other, and a crash can come between two.
            let mut done = 0;
            while done < length {
                let count = (CLUSTER - (offset + done) % CLUSTER).min(length - done);
                let target = &mut model[offset + done..offset + done + count];
                match &data {
                    Some(data) => target.copy_from_slice(&data[done..done + count]),
                    None => target.fill(0),
                }
                moments.push(checksum(&model));
                done += count;
            }
            match random.below(12) {
                0 => {
                    layer.flush().unwrap();
                    moments = vec![checksum(&model)];
                }
                // A commit that gets only so far.
                1 => {
                    let step = [
                        CommitStep::MapWritten,
                        CommitStep::MapSynchronized,
                        CommitStep::SuperblockWritten,
                    ][random.below(3)];
                    layer.set_commit_fault(Some(step));
                    // With nothing changed since the last commit there is nothing to fail at.
                    if layer.flush().is_ok() {
                        moments = vec![checksum(&model)];
                    }
                    layer.set_commit_fault(None);
                }
                _ => {}
            }
        }
        drop(layer);

        let mut crashed = memory.crash(&mut random);
        // Sometimes the host goes down again while the next open is tidying up after the first.
        if seed % 3 == 0 {
            let step = [
                CommitStep::MapWritten,
                CommitStep::MapSynchronized,
                CommitStep::SuperblockWritten,
            ][random.below(3)];
            assert!(Layer::open_with(Box::new(crashed.clone()), &quiet(), None, Some(step)).is_err());
            crashed = crashed.crash(&mut random);
        }
        let layer = Layer::open_storage(Box::new(crashed.clone()), &quiet(), None).unwrap();
        let found = contents(&layer);
        assert!(moments.contains(&checksum(&found)), "seed {seed}");
        assert!(layer.verify().unwrap().is_empty(), "seed {seed}");

        // And it carries on from there as any layer does.
        layer.write(5, b"after").unwrap();
        let expected = contents(&layer);
        layer.close().unwrap();
        let layer = Layer::open_storage(Box::new(crashed.clone()), &quiet(), None).unwrap();
        assert!(contents(&layer) == expected, "seed {seed}");
        let layer = {
            drop(layer);
            Layer::open_storage(Box::new(crashed.crash(&mut random)), &quiet(), None).unwrap()
        };
        assert!(contents(&layer) == expected, "seed {seed}");
    }
}
