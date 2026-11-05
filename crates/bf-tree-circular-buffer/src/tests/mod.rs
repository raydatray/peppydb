use std::any::Any;
use std::sync::Mutex;

use bf_tree_node_id::LeafId;

use super::*;

mod alloc;
mod claims;
mod holes;

fn leaf(raw: u64) -> LeafId {
    LeafId::new(raw).unwrap()
}

/// Keep a value reachable for the rest of the test process. Tests that leak a
/// slab on purpose park the buffer or block here, so Miri does not report the
/// intended leak.
fn keep_alive(value: impl Any + Send) {
    static KEPT: Mutex<Vec<Box<dyn Any + Send>>> = Mutex::new(Vec::new());
    KEPT.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Box::new(value));
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
