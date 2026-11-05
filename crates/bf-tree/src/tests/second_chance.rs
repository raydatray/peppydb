//! The second chance: aging blocks are copied to the tail on access.

use super::*;
use crate::leaf_map::Locked;

fn with_percent(percent: u8) -> TreeConfig {
    TreeConfig {
        circular_buffer_bytes: 2048,
        second_chance_percent: percent,
        ..config(256, 128, 8, 16)
    }
}

fn leaf0() -> LeafId {
    LeafId::new(0).unwrap()
}

fn addr(tree: &Tree, id: LeafId) -> Option<usize> {
    tree.leaves.read(id).unwrap().block_addr()
}

#[test]
fn percent_above_100_is_rejected() {
    assert!(matches!(
        with_percent(101).validate(),
        Err(TreeError::InvalidConfig(_))
    ));
    assert_eq!(with_percent(100).validate(), Ok(()));
    assert_eq!(with_percent(0).validate(), Ok(()));
}

#[test]
fn at_100_every_read_relocates_and_at_0_none_does() {
    for (percent, moves) in [(100u8, true), (0, false)] {
        let tree = Tree::in_memory(with_percent(percent)).unwrap();
        tree.put(b"a", b"1").unwrap();
        let mut last = addr(&tree, leaf0()).unwrap();
        for _ in 0..5 {
            assert_eq!(tree.get(b"a"), Ok(Some(b"1".to_vec())));
            let now = addr(&tree, leaf0()).unwrap();
            assert_eq!(now > last, moves, "percent {percent}");
            assert!(now >= last);
            last = now;
        }
        assert_eq!(tree.check_invariants(), Ok(()));
    }
}

#[test]
fn relocation_never_evicts() {
    let tree = Tree::in_memory(with_percent(100)).unwrap();
    tree.put(b"a", b"1").unwrap();
    let before = tree.leaves.circular_buffer.metrics();
    for _ in 0..20 {
        tree.get(b"a").unwrap();
    }
    let after = tree.leaves.circular_buffer.metrics();
    assert_eq!(after.retired, before.retired, "no evictions");
    assert!(after.allocs > before.allocs, "copies were made");
    assert_eq!(tree.buffered_leaves(), 1);
    assert_eq!(tree.get(b"a"), Ok(Some(b"1".to_vec())));
}

#[test]
fn a_hint_is_skipped_while_another_reader_holds_the_leaf() {
    let tree = Tree::in_memory(with_percent(100)).unwrap();
    tree.put(b"a", b"1").unwrap();
    let before = addr(&tree, leaf0());
    let other = tree.leaves.read(leaf0()).unwrap();
    assert_eq!(tree.get(b"a"), Ok(Some(b"1".to_vec())));
    assert_eq!(
        addr_under(&other),
        before,
        "the reader did not wait or relocate"
    );
    drop(other);
    tree.get(b"a").unwrap();
    assert_ne!(addr(&tree, leaf0()), before, "uncontended, it relocates");
}

fn addr_under(leaf: &impl Locked) -> Option<usize> {
    leaf.block_addr()
}

#[test]
fn a_hint_is_skipped_when_the_leaf_changed() {
    let tree = Tree::in_memory(with_percent(100)).unwrap();
    tree.put(b"a", b"1").unwrap();
    let noted = tree.note(&tree.leaves.read(leaf0()).unwrap());
    assert!(noted.is_some());
    // A write changes the block (and relocates it itself).
    tree.put(b"b", b"2").unwrap();
    let after_write = addr(&tree, leaf0());
    tree.maintain(noted).unwrap();
    assert_eq!(
        addr(&tree, leaf0()),
        after_write,
        "the stale hint did nothing"
    );
}

#[test]
fn a_hot_leaf_survives_while_cold_leaves_are_evicted() {
    let tree = Tree::in_memory(with_percent(50)).unwrap();
    // Several leaves, none buffered.
    for i in 0..300u16 {
        tree.put(&(i * 211).to_be_bytes(), &[1; 8]).unwrap();
        if i % 30 == 29 {
            tree.flush().unwrap();
        }
    }
    tree.flush().unwrap();
    let hot_key = 0u16.to_be_bytes();
    tree.put(&hot_key, b"hot").unwrap();
    let hot = tree.find_leaf(&hot_key);
    let first = addr(&tree, hot).unwrap();

    // Cold writes cycle the buffer; the hot key is read between them.
    let retired_before = tree.leaves.circular_buffer.metrics().retired;
    for round in 0..3000u16 {
        let cold = (round.wrapping_mul(97) | 1).to_be_bytes();
        tree.put(&cold, b"cold").unwrap();
        assert_eq!(tree.get(&hot_key), Ok(Some(b"hot".to_vec())));
    }
    let retired = tree.leaves.circular_buffer.metrics().retired - retired_before;
    assert!(retired > 50, "cold blocks were evicted: {retired}");
    let last = addr(&tree, hot).expect("the hot leaf is still buffered");
    assert!(last > first, "the hot block moved to the tail");
    assert_eq!(tree.check_invariants(), Ok(()));
}
