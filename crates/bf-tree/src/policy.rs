//! The caching policy: promotion rates and a per-tree random generator.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// Tree identities, never reused, unlike addresses.
static NEXT_TREE: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The tree this thread's generator belongs to (0: none) and its state.
    static RNG: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

/// The random decisions of one tree.
///
/// A tree's rolls depend only on its own seed: the first thread to roll for
/// a fresh tree gets stream 0, so a single-threaded run is reproducible
/// whatever other trees the thread used before. The read path writes no
/// shared cache line; the atomic is touched only when a thread switches
/// trees. Correctness never depends on a roll.
#[derive(Debug)]
pub(super) struct Policy {
    seed: u64,
    tree: u64,
    /// Generator streams handed to threads, one per first use.
    streams: AtomicU64,
}

impl Policy {
    pub(super) fn new(seed: u64) -> Self {
        Self {
            seed,
            tree: NEXT_TREE.fetch_add(1, Ordering::Relaxed),
            streams: AtomicU64::new(0),
        }
    }

    /// True with probability `percent` / 100. 0 and 100 draw nothing.
    pub(super) fn roll(&self, percent: u8) -> bool {
        match percent {
            0 => false,
            100.. => true,
            _ => {
                let x = RNG.with(|cell| {
                    let (tree, mut state) = cell.get();
                    if tree != self.tree {
                        let stream = self.streams.fetch_add(1, Ordering::Relaxed);
                        state = splitmix64(self.seed ^ splitmix64(stream));
                    }
                    let out = splitmix64(state);
                    state = state.wrapping_add(GOLDEN);
                    cell.set((self.tree, state));
                    out
                });
                ((x >> 32) * 100) >> 32 < u64::from(percent)
            }
        }
    }
}

const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(GOLDEN);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
