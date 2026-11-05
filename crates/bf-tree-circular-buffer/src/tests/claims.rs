use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;

const H: usize = BLOCK_HEADER_BYTES;

#[test]
fn victims_are_claimed_oldest_first_past_dead_blocks() {
    // With percent 0 the dead block in the middle is a hole; with 100 it is
    // a tombstone. Either way the claim passes it, and the filler too.
    for percent in [0, 100] {
        let buffer = CircularBuffer::new(512, &[64, 192], percent).unwrap();
        let x = buffer.alloc(192, Kind::Mini, leaf(9)).unwrap();
        let y = buffer.alloc(192, Kind::Mini, leaf(9)).unwrap();
        buffer.free(x);
        buffer.free(y);
        // Head and tail at 416. `a` ends at 496; `b` would cross 512, so a
        // 16-byte filler precedes it.
        let a = buffer.alloc(64, Kind::Mini, leaf(0)).unwrap();
        let b = buffer.alloc(64, Kind::Full, leaf(1)).unwrap();
        let dead = buffer.alloc(64, Kind::Mini, leaf(2)).unwrap();
        let c = buffer.alloc(64, Kind::Mini, leaf(3)).unwrap();
        assert_eq!(buffer.metrics().fillers, 1);
        buffer.free(dead);

        let victims: Vec<_> = (0..3).map(|_| victim(buffer.claim_victim())).collect();
        let order: Vec<_> = victims.iter().map(|v| (v.owner, v.kind)).collect();
        assert_eq!(
            order,
            [
                (leaf(0), Kind::Mini),
                (leaf(1), Kind::Full),
                (leaf(3), Kind::Mini)
            ],
            "percent {percent}"
        );
        assert_eq!(buffer.claim_victim(), Claim::Busy);

        for (block, v) in [a, b, c].into_iter().zip(victims) {
            assert!(block.is(&v));
            buffer.free(block);
            buffer.retire(v);
        }
        assert_eq!(buffer.used(), 0);
        assert_eq!(buffer.claim_victim(), Claim::Empty);
    }
}

#[test]
fn two_claimants_receive_distinct_victims() {
    let buffer = CircularBuffer::new(1024, &[64], 0).unwrap();
    let a = buffer.alloc(64, Kind::Mini, leaf(1)).unwrap();
    let b = buffer.alloc(64, Kind::Mini, leaf(2)).unwrap();

    let first = victim(buffer.claim_victim());
    let second = victim(buffer.claim_victim());
    assert_ne!(first, second);
    assert!(a.is(&first) && b.is(&second));
    assert!(!a.is(&second));
    assert_eq!(buffer.claim_victim(), Claim::Busy);

    buffer.free(a);
    buffer.retire(first);
    buffer.free(b);
    buffer.retire(second);
    assert_eq!(buffer.used(), 0);
    assert_eq!(buffer.metrics().retired, 2);
}

#[test]
fn a_retire_behind_the_head_makes_room_only_when_the_head_reaches_it() {
    let buffer = CircularBuffer::new(512, &[112], 0).unwrap();
    let a = buffer.alloc(112, Kind::Mini, leaf(1)).unwrap();
    let b = buffer.alloc(112, Kind::Mini, leaf(2)).unwrap();
    let slow = victim(buffer.claim_victim());
    let fast = victim(buffer.claim_victim());
    assert!(a.is(&slow) && b.is(&fast));

    // The second evictor finishes first: no room while the first holds the head.
    buffer.free(b);
    buffer.retire(fast);
    assert_eq!(buffer.used(), 2 * (H + 112));

    // When the first retires, the head passes both.
    buffer.free(a);
    buffer.retire(slow);
    assert_eq!(buffer.used(), 0);
}

#[test]
fn a_block_freed_while_claimed_is_never_reused() {
    let buffer = CircularBuffer::new(1024, &[64], 0).unwrap();
    let head = buffer.alloc(64, Kind::Mini, leaf(0)).unwrap();
    let claimed = buffer.alloc(64, Kind::Mini, leaf(1)).unwrap();
    let after = buffer.alloc(64, Kind::Mini, leaf(2)).unwrap();
    let claimed_addr = claimed.addr();

    // Claim `head` and `claimed`, then give `head` back so `claimed` is the
    // one held, behind a live head.
    let first = victim(buffer.claim_victim());
    let v = victim(buffer.claim_victim());
    buffer.unclaim(first);
    assert!(claimed.is(&v));

    // The owner frees the block during the eviction. It is not a hole.
    buffer.free(claimed);
    let next = buffer.alloc(64, Kind::Mini, leaf(3)).unwrap();
    assert_ne!(next.addr(), claimed_addr);
    assert_eq!(buffer.metrics().reuses, 0);

    // After retire and the head's block leaving, the head passes it.
    buffer.retire(v);
    buffer.free(head);
    assert_eq!(buffer.used(), 2 * (H + 64));
    buffer.free(after);
    buffer.free(next);
    assert_eq!(buffer.used(), 0);
}

#[test]
fn unclaim_returns_a_live_block_or_retires_a_freed_one() {
    let buffer = CircularBuffer::new(1024, &[64], 0).unwrap();
    let a = buffer.alloc(64, Kind::Mini, leaf(1)).unwrap();

    // Not freed: the block is live again and can be claimed again.
    let v = victim(buffer.claim_victim());
    buffer.unclaim(v);
    let again = victim(buffer.claim_victim());
    assert_eq!(v, again);

    // Freed during the claim: unclaim retires it.
    buffer.free(a);
    buffer.unclaim(again);
    assert_eq!(buffer.used(), 0);
    assert_eq!(buffer.metrics().retired, 1);
    assert_eq!(buffer.claim_victim(), Claim::Empty);
}

#[test]
fn retire_without_free_panics_and_changes_nothing() {
    let buffer = CircularBuffer::new(1024, &[64], 0).unwrap();
    let block = buffer.alloc(64, Kind::Mini, leaf(1)).unwrap();
    let v = victim(buffer.claim_victim());

    let result = catch_unwind(AssertUnwindSafe(|| buffer.retire(v)));
    assert!(result.is_err());

    // The claim stands; the eviction can still finish properly.
    assert_eq!(buffer.used(), H + 64);
    buffer.free(block);
    buffer.retire(v);
    assert_eq!(buffer.used(), 0);
}

#[test]
fn busy_versus_empty() {
    let buffer = CircularBuffer::new(1024, &[64], 0).unwrap();
    assert_eq!(buffer.claim_victim(), Claim::Empty);

    let block = buffer.alloc(64, Kind::Mini, leaf(1)).unwrap();
    let v = victim(buffer.claim_victim());
    assert_eq!(buffer.claim_victim(), Claim::Busy);

    buffer.free(block);
    assert_eq!(buffer.claim_victim(), Claim::Busy, "freed but not retired");
    buffer.retire(v);
    assert_eq!(buffer.claim_victim(), Claim::Empty);
}

#[test]
fn a_reused_hole_records_its_new_owner() {
    let buffer = CircularBuffer::new(1024, &[64], 0).unwrap();
    let a = buffer.alloc(64, Kind::Mini, leaf(1)).unwrap();
    let b = buffer.alloc(64, Kind::Mini, leaf(2)).unwrap();
    buffer.free(b);
    let c = buffer.alloc(64, Kind::Full, leaf(3)).unwrap();

    let first = victim(buffer.claim_victim());
    let second = victim(buffer.claim_victim());
    assert_eq!((first.owner, first.kind), (leaf(1), Kind::Mini));
    assert_eq!((second.owner, second.kind), (leaf(3), Kind::Full));

    buffer.free(a);
    buffer.retire(first);
    buffer.free(c);
    buffer.retire(second);
    assert_eq!(buffer.used(), 0);
}
