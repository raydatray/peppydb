use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::Mutex;

/// Chunk `c` holds `FIRST << c` entries, so 48 chunks address every index a
/// 64-bit `usize` allows in practice.
const FIRST: usize = 64;
const CHUNKS: usize = 48;

/// An append-only table that grows while other threads read it.
///
/// Entries live in chunks that are never moved or freed, so a `&T` from
/// [`Table::get`] stays valid for the life of the table, and `get` takes no
/// lock. Growth is [`Table::grow_to`] rather than a push, because callers
/// index the table with IDs chosen elsewhere that may arrive out of order.
#[derive(Debug)]
pub struct Table<T> {
    chunks: Box<[OnceLock<Box<[T]>>]>,
    /// Number of valid indices. Published with `Release` after the chunks
    /// that hold them are initialized.
    len: AtomicUsize,
    grow: Mutex<()>,
}

impl<T: Default> Default for Table<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Default> Table<T> {
    pub fn new() -> Self {
        Self {
            chunks: (0..CHUNKS).map(|_| OnceLock::new()).collect(),
            len: AtomicUsize::new(0),
            grow: Mutex::new(()),
        }
    }

    /// Number of valid indices: `0..len()`.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entry at `index`, or `None` at or beyond `len()`. Lock-free.
    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len() {
            return None;
        }
        let (chunk, offset) = locate(index);
        let chunk = self.chunks[chunk].get().expect("chunks below len exist");
        Some(&chunk[offset])
    }

    /// Make every index below `len` valid, filling new entries with
    /// `T::default()`. Idempotent; growth is serialized, and `len` is
    /// published after the entries exist.
    pub fn grow_to(&self, len: usize) {
        if len <= self.len() {
            return;
        }
        let _guard = self.grow.lock();
        if len <= self.len() {
            return;
        }
        let (last, _) = locate(len - 1);
        for (chunk, cell) in self.chunks.iter().enumerate().take(last + 1) {
            cell.get_or_init(|| (0..FIRST << chunk).map(|_| T::default()).collect());
        }
        self.len.store(len, Ordering::Release);
    }

    /// The valid entries in index order.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        (0..self.len()).map(|index| self.get(index).expect("index below len"))
    }
}

/// The chunk and offset of `index`. Chunk `c` starts at `FIRST * (2^c - 1)`.
pub(crate) fn locate(index: usize) -> (usize, usize) {
    let scaled = index / FIRST + 1;
    let chunk = (usize::BITS - 1 - scaled.leading_zeros()) as usize;
    let start = FIRST * ((1 << chunk) - 1);
    (chunk, index - start)
}
