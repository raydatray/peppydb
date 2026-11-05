use bf_tree_sync::Mutex;
use std::any::Any;

use bf_tree_node_id::LeafId;

use super::*;

mod alloc;
mod bench;
mod claims;
mod holes;
mod model;
mod stress;

fn leaf(raw: u64) -> LeafId {
    LeafId::new(raw).unwrap()
}

/// Keep a value reachable for the rest of the test process. Tests that leak a
/// slab on purpose park the buffer or block here, so Miri does not report the
/// intended leak.
fn keep_alive(value: impl Any + Send) {
    static KEPT: Mutex<Vec<Box<dyn Any + Send>>> = Mutex::new(Vec::new());
    KEPT.lock().push(Box::new(value));
}

/// The block's offset from the start of the slab, header included.
fn offset(buffer: &CircularBuffer, block: &Block) -> usize {
    block.ptr().as_ptr().addr() - buffer.slab.range(0, 0).as_ptr().addr()
}

/// The victim of a claim that must find one.
fn victim(claim: Claim) -> Victim {
    match claim {
        Claim::Victim(victim) => victim,
        other => panic!("expected a victim, got {other:?}"),
    }
}

/// A small deterministic generator (SplitMix64), so tests need no
/// dependency and every failure reproduces from its seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`; `n` must be nonzero.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}
