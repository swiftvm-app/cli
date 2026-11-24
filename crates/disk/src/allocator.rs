//! Which 4 KiB blocks of a layer's file are in use. Nothing of this is stored: it is worked out
//! from the map when the layer is opened.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Run {
    pub block: usize,
    pub count: usize,
}

pub(crate) struct Allocator {
    /// The first block that can be handed out. What lies before it has a fixed purpose.
    start: usize,
    /// One past the last block that was ever handed out.
    pub end: usize,
    used: Vec<u64>,
    /// Free blocks that the file system may still hold storage for.
    stale: Vec<u64>,
    cursor: usize,
}

fn is_set(bits: &[u64], index: usize) -> bool {
    bits.get(index >> 6)
        .is_some_and(|word| word & 1 << (index & 63) != 0)
}

fn set(bits: &mut [u64], index: usize) {
    bits[index >> 6] |= 1 << (index & 63);
}

fn clear(bits: &mut [u64], index: usize) {
    bits[index >> 6] &= !(1 << (index & 63));
}

impl Allocator {
    pub fn new(start: usize) -> Self {
        Self {
            start,
            end: start,
            used: Vec::new(),
            stale: Vec::new(),
            cursor: start,
        }
    }

    fn reserve(&mut self, through: usize) {
        let words = (through >> 6) + 1;
        if words > self.used.len() {
            self.used.resize(words, 0);
            self.stale.resize(words, 0);
        }
    }

    #[cfg(test)]
    pub fn is_used(&self, block: usize) -> bool {
        is_set(&self.used, block)
    }

    /// Takes blocks that are known to be in use, while the layer is being opened.
    pub fn mark_used(&mut self, block: usize, count: usize) {
        if count == 0 {
            return;
        }
        self.reserve(block + count - 1);
        for index in block..block + count {
            set(&mut self.used, index);
            clear(&mut self.stale, index);
        }
        self.end = self.end.max(block + count);
    }

    /// Hands out `count` blocks in a row: the first gap that fits, or new blocks at the end.
    pub fn allocate(&mut self, count: usize) -> usize {
        let mut index = self.cursor;
        while index + count <= self.end {
            if index & 63 == 0 && self.used[index >> 6] == u64::MAX {
                index += 64;
                continue;
            }
            if is_set(&self.used, index) {
                index += 1;
                continue;
            }
            let mut run = 1;
            while run < count && !is_set(&self.used, index + run) {
                run += 1;
            }
            if run == count {
                self.mark_used(index, count);
                self.cursor = index + count;
                return index;
            }
            index += run + 1;
        }
        let block = self.end;
        self.mark_used(block, count);
        self.cursor = self.end;
        block
    }

    /// Gives blocks back. They can be handed out again right away, so this is only called once
    /// nothing that could still be read after a crash points to them.
    pub fn release(&mut self, block: usize, count: usize) {
        for index in block..block + count {
            clear(&mut self.used, index);
            set(&mut self.stale, index);
        }
        self.cursor = self.cursor.min(block);
    }

    /// Lets go of the free blocks at the end, and says where the end is now if it has moved. The
    /// file is cut off there: nothing may be handed out again before it is.
    pub fn trim_end(&mut self) -> Option<usize> {
        let mut last = self.end;
        while last > self.start {
            if last & 63 == 0 && self.used[(last - 1) >> 6] == 0 {
                last = (last - 64).max(self.start);
            } else if !is_set(&self.used, last - 1) {
                last -= 1;
            } else {
                break;
            }
        }
        if last >= self.end {
            return None;
        }
        for index in last..self.end {
            clear(&mut self.stale, index);
        }
        self.end = last;
        self.cursor = self.cursor.min(self.end);
        Some(self.end)
    }

    /// Calls every free block stale: after a crash nothing says which of them still take up room.
    pub fn mark_all_free_stale(&mut self) {
        for index in self.start..self.end {
            if !is_set(&self.used, index) {
                set(&mut self.stale, index);
            }
        }
    }

    /// The next run of at least `minimum` stale blocks from `position` on, taken out of
    /// circulation until [`Allocator::finish_reclaiming`] so that nobody is handed a block while
    /// its storage is being given back. Shorter runs stay stale.
    pub fn take_stale_run(&mut self, position: usize, minimum: usize) -> Option<Run> {
        const LIMIT: usize = 1 << 16;
        let mut index = position.max(self.start);
        while index < self.end {
            if index & 63 == 0 && self.stale[index >> 6] == 0 {
                index += 64;
                continue;
            }
            if !is_set(&self.stale, index) {
                index += 1;
                continue;
            }
            let mut count = 1;
            while count < LIMIT && index + count < self.end && is_set(&self.stale, index + count) {
                count += 1;
            }
            if count < minimum {
                index += count;
                continue;
            }
            self.mark_used(index, count);
            return Some(Run { block: index, count });
        }
        None
    }

    pub fn finish_reclaiming(&mut self, run: Run) {
        for index in run.block..run.block + run.count {
            clear(&mut self.used, index);
        }
        self.cursor = self.cursor.min(run.block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hands_out_gaps_first_and_trims_the_end() {
        let mut allocator = Allocator::new(10);
        assert_eq!(allocator.allocate(3), 10);
        assert_eq!(allocator.allocate(2), 13);
        assert_eq!(allocator.allocate(1), 15);
        allocator.release(10, 3);
        assert_eq!(allocator.allocate(2), 10);
        // Too small a gap for four: those go at the end.
        assert_eq!(allocator.allocate(4), 16);
        allocator.release(16, 4);
        assert_eq!(allocator.trim_end(), Some(16));
        assert_eq!(allocator.end, 16);

        let run = allocator.take_stale_run(0, 1).unwrap();
        assert_eq!(run, Run { block: 12, count: 1 });
        assert!(allocator.is_used(12));
        allocator.finish_reclaiming(run);
        assert!(!allocator.is_used(12));
        assert_eq!(allocator.take_stale_run(0, 1), None);
    }
}
