use std::sync::atomic::{AtomicU64, Ordering};

/// One reference bit per slot index of a mini-page. A page holds at most
/// `(4096 - 32) / 8 = 508` slots, so 512 bits suffice.
///
/// The bits live beside the page rather than in it, so a reader holding only
/// a shared leaf lock can mark a record through `&self`. Marks use `Relaxed`
/// ordering: the leaf lock orders a reader's marks before a later writer's
/// reads, and concurrent marks commute.
#[derive(Debug, Default)]
pub struct RefBits([AtomicU64; 8]);

const BITS: usize = 512;

impl RefBits {
    /// Set the bit of slot `index`. Loads first and writes only if the bit is
    /// clear, so repeated hits on a hot record do not write its cache line.
    pub fn mark(&self, index: usize) {
        let (word, bit) = position(index);
        let word = &self.0[word];
        if word.load(Ordering::Relaxed) & bit == 0 {
            word.fetch_or(bit, Ordering::Relaxed);
        }
    }

    pub fn is_set(&self, index: usize) -> bool {
        let (word, bit) = position(index);
        self.0[word].load(Ordering::Relaxed) & bit != 0
    }

    /// Open a clear bit at `index` and move every bit at or above it up by
    /// one, following a slot inserted at `index`. The bit at 511 falls off;
    /// no page has that many slots.
    pub fn insert(&mut self, index: usize) {
        let (word, bit) = position(index);
        let words = self.0.each_mut().map(|word| *word.get_mut());
        let mut shifted = words;
        for i in (word + 1..words.len()).rev() {
            shifted[i] = (words[i] << 1) | (words[i - 1] >> 63);
        }
        let low = words[word] & (bit - 1);
        let high = words[word] & !(bit - 1);
        shifted[word] = low | (high << 1);
        for (atomic, value) in self.0.iter_mut().zip(shifted) {
            *atomic.get_mut() = value;
        }
    }

    pub fn clear(&mut self) {
        for word in &mut self.0 {
            *word.get_mut() = 0;
        }
    }
}

/// The word index and bit mask of slot `index`. Panics past 511.
fn position(index: usize) -> (usize, u64) {
    assert!(
        index < BITS,
        "slot index {index} exceeds the reference bits"
    );
    (index / 64, 1 << (index % 64))
}
