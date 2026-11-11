//! Read promotion: base-served reads keep the whole page as a full page.

use std::collections::BTreeMap;

use super::store::counting;
use super::*;
use crate::leaf_map::Residency;

/// Page mode: every base-served read promotes.
fn config(leaf: usize, inner: usize, key: usize, value: usize) -> TreeConfig {
    TreeConfig {
        read_promotion_percent: 100,
        read_record_cache: false,
        ..super::config(leaf, inner, key, value)
    }
}

fn residency(tree: &Tree, id: LeafId) -> &'static str {
    use crate::leaf_map::Locked;
    match tree.leaves.read(id).unwrap().residency() {
        Residency::Base => "base",
        Residency::Mini { .. } => "mini",
        Residency::Full(_) => "full",
    }
}

fn leaf_of(tree: &Tree, key: &[u8]) -> LeafId {
    tree.find_leaf(key)
}

fn key(i: u32) -> [u8; 4] {
    i.to_be_bytes()
}

#[test]
fn a_promoting_read_keeps_the_page_and_the_next_read_loads_nothing() {
    let config = config(512, 256, 8, 16);
    let (store, io) = counting(config);
    let tree = Tree::create(config, store).unwrap();
    for i in 0..10 {
        tree.put(&key(i), b"v").unwrap();
    }
    tree.checkpoint().unwrap();
    let leaf = leaf_of(&tree, &key(3));
    assert_eq!(residency(&tree, leaf), "base");
    io.take();

    assert_eq!(tree.get(&key(3)), Ok(Some(b"v".to_vec())));
    assert_eq!(residency(&tree, leaf), "full");
    // The read and the promotion each load the page (a known cost).
    assert_eq!(io.take(), (2, vec![]), "no write");
    assert_eq!(tree.get(&key(4)), Ok(Some(b"v".to_vec())));
    assert_eq!(tree.get(&key(99)), Ok(None));
    assert_eq!(io.take(), (0, vec![]), "the full page served both");
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn a_mini_page_leaf_promotes_by_merging_in_place() {
    let tree = Tree::in_memory(config(512, 256, 8, 16)).unwrap();
    for i in 0..4 {
        tree.put(&key(i), b"v").unwrap();
    }
    let leaf = leaf_of(&tree, &key(0));
    assert_eq!(residency(&tree, leaf), "mini");
    // An absent key is served by the base page.
    assert_eq!(tree.get(&key(50)), Ok(None));
    assert_eq!(residency(&tree, leaf), "full");
    assert_eq!(tree.leaf_count(), 1);
    assert_eq!(tree.buffered_leaves(), 0);
    for i in 0..4 {
        assert_eq!(tree.get(&key(i)), Ok(Some(b"v".to_vec())));
    }
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn a_mini_page_whose_merge_overflows_splits_and_keeps_the_left_half() {
    let config = config(512, 256, 8, 16);
    // The most keys one flushed leaf holds.
    let fits = |n: u32| {
        let tree = Tree::in_memory(config).unwrap();
        (0..n).for_each(|i| tree.put(&key(i * 2), &[1; 16]).unwrap());
        tree.checkpoint().unwrap();
        tree.leaf_count() == 1
    };
    let most = (1..200).take_while(|&n| fits(n)).last().unwrap();

    let tree = Tree::in_memory(config).unwrap();
    let mut oracle = BTreeMap::new();
    for i in 0..most {
        tree.put(&key(i * 2), &[1; 16]).unwrap();
        oracle.insert(key(i * 2).to_vec(), vec![1; 16]);
    }
    tree.checkpoint().unwrap();
    // Buffer new keys: their merge no longer fits one page.
    for i in 0..3 {
        tree.put(&key(i * 2 + 1), &[2; 16]).unwrap();
        oracle.insert(key(i * 2 + 1).to_vec(), vec![2; 16]);
    }
    let leaf = LeafId::new(0).unwrap();
    assert_eq!((tree.leaf_count(), residency(&tree, leaf)), (1, "mini"));

    // An absent key is served by the base page.
    assert_eq!(tree.get(&key(1000)), Ok(None));
    assert_eq!(tree.leaf_count(), 2, "the promotion split the leaf");
    assert_eq!(residency(&tree, leaf), "full", "the left half is kept");
    assert_eq!(tree.buffered_leaves(), 0);
    for (k, v) in &oracle {
        assert_eq!(tree.get(k).unwrap().as_ref(), Some(v));
    }
    assert_eq!(tree.iter().count(), oracle.len());
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn deferral_evicts_clean_full_pages_without_io() {
    let config = TreeConfig {
        circular_buffer_bytes: 2048,
        ..config(512, 256, 8, 16)
    };
    let (store, io) = counting(config);
    let tree = Tree::create(config, store).unwrap();
    for i in 0..400 {
        tree.put(&key(i), b"v").unwrap();
    }
    tree.checkpoint().unwrap();
    // Evict whatever the writes left in the buffer.
    while tree.evict_one() == Ok(crate::evict::Eviction::Evicted) {}
    let leaves: Vec<_> = (0..400)
        .step_by(10)
        .map(|i| (i, leaf_of(&tree, &key(i))))
        .collect();
    let mut distinct = leaves.clone();
    distinct.dedup_by_key(|(_, leaf)| *leaf);
    assert!(distinct.len() > 4, "more leaves than the buffer holds");
    io.take();
    for (i, leaf) in &distinct {
        assert_eq!(tree.get(&key(*i)), Ok(Some(b"v".to_vec())));
        assert_eq!(residency(&tree, *leaf), "full", "promoted after deferral");
    }
    assert_eq!(io.take().1, Vec::<LeafId>::new(), "no write-back");
    let full = distinct
        .iter()
        .filter(|(_, leaf)| residency(&tree, *leaf) == "full")
        .count();
    assert!(full < distinct.len(), "earlier full pages were evicted");
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn deferral_stops_at_a_dirty_mini_page_and_unclaims_it() {
    let config = TreeConfig {
        circular_buffer_bytes: 2048,
        ..config(512, 256, 8, 16)
    };
    let (store, io) = counting(config);
    let tree = Tree::create(config, store).unwrap();
    for i in 0..400 {
        tree.put(&key(i), b"v").unwrap();
    }
    tree.checkpoint().unwrap();
    while tree.evict_one() == Ok(crate::evict::Eviction::Evicted) {}
    // The oldest block: a dirty mini-page.
    tree.put(&key(0), b"dirty").unwrap();
    let dirty = leaf_of(&tree, &key(0));
    assert_eq!(residency(&tree, dirty), "mini");
    io.take();
    let mut base_left = false;
    for i in (10..400).step_by(10) {
        let leaf = leaf_of(&tree, &key(i));
        if leaf == dirty {
            continue;
        }
        assert_eq!(tree.get(&key(i)), Ok(Some(b"v".to_vec())));
        base_left |= residency(&tree, leaf) == "base";
    }
    assert!(base_left, "some promotion found no room");
    assert_eq!(residency(&tree, dirty), "mini", "the dirty mini-page stays");
    assert_eq!(io.take().1, Vec::<LeafId>::new(), "no write-back");
    assert_eq!(tree.get(&key(0)), Ok(Some(b"dirty".to_vec())));
    // The claim was released: a real eviction can still take it.
    assert_eq!(tree.evict_one(), Ok(crate::evict::Eviction::Evicted));
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn a_stale_promotion_is_skipped() {
    let tree = Tree::in_memory(config(512, 256, 8, 16)).unwrap();
    tree.put(&key(1), b"v").unwrap();
    tree.checkpoint().unwrap();
    let leaf = leaf_of(&tree, &key(1));
    let noted = tree
        .note(&tree.leaves.read(leaf).unwrap())
        .with_hint(Some(crate::maintain::Hint::Promote));
    tree.put(&key(2), b"w").unwrap();
    tree.maintain(noted).unwrap();
    assert_eq!(residency(&tree, leaf), "mini", "the hint did nothing");
}

#[test]
fn rolls_depend_only_on_the_seed() {
    let rolls = |tree: &Tree| -> Vec<bool> { (0..64).map(|_| tree.policy.roll(50)).collect() };
    let config = TreeConfig {
        policy_seed: 42,
        ..config(512, 256, 8, 16)
    };
    let first = rolls(&Tree::in_memory(config).unwrap());
    // Another tree rolls on this thread in between.
    let other = Tree::in_memory(TreeConfig {
        policy_seed: 7,
        ..config
    })
    .unwrap();
    let _ = rolls(&other);
    let again = Tree::in_memory(config).unwrap();
    let _ = rolls(&other);
    assert_eq!(rolls(&again), first);
    assert_ne!(rolls(&other), first);
}

#[test]
fn a_rate_of_twenty_lands_near_twenty_percent() {
    let tree = Tree::in_memory(config(512, 256, 8, 16)).unwrap();
    let hits = (0..10_000).filter(|_| tree.policy.roll(20)).count();
    assert!((1700..2300).contains(&hits), "{hits}");
    assert!(!(0..100).any(|_| tree.policy.roll(0)));
    assert!((0..100).all(|_| tree.policy.roll(100)));
}

fn scanning(percent: u8) -> TreeConfig {
    TreeConfig {
        scan_promotion_percent: percent,
        read_promotion_percent: 0,
        ..config(512, 256, 8, 16)
    }
}

/// A tree over a counting store with flushed keys spread over many leaves,
/// and an empty circular buffer.
fn flushed(config: TreeConfig) -> (Tree, super::store::Io) {
    let (store, io) = counting(config);
    let tree = Tree::create(config, store).unwrap();
    for i in 0..300 {
        tree.put(&key(i), &[1; 8]).unwrap();
    }
    tree.checkpoint().unwrap();
    while tree.evict_one() == Ok(crate::evict::Eviction::Evicted) {}
    io.take();
    (tree, io)
}

fn leaves(tree: &Tree) -> Vec<LeafId> {
    let mut leaves: Vec<_> = (0..300).map(|i| leaf_of(tree, &key(i))).collect();
    leaves.dedup();
    leaves
}

#[test]
fn at_scan_rate_100_every_scanned_leaf_ends_full() {
    let (tree, io) = flushed(scanning(100));
    assert_eq!(tree.iter().count(), 300);
    let leaves = leaves(&tree);
    assert!(leaves.len() > 2);
    assert!(leaves.iter().all(|&leaf| residency(&tree, leaf) == "full"));
    io.take();
    // The second scan (and both scan forms) read no base page.
    assert_eq!(tree.iter().count(), 300);
    let mut seen = 0;
    tree.scan_with(None, None, |_, _| {
        seen += 1;
        std::ops::ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(seen, 300);
    assert_eq!(io.take(), (0, vec![]));
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn at_scan_rate_0_a_scan_allocates_nothing() {
    let (tree, _io) = flushed(scanning(0));
    let before = tree.circular_buffer_used();
    assert_eq!(tree.iter().count(), 300);
    assert_eq!(tree.circular_buffer_used(), before);
    assert!(
        leaves(&tree)
            .iter()
            .all(|&leaf| residency(&tree, leaf) == "base")
    );
}

#[test]
fn a_scan_over_mini_page_leaves_writes_nothing_and_leaves_them_mini() {
    let (tree, io) = flushed(scanning(100));
    let leaves = leaves(&tree);
    // Buffer one write in every leaf.
    for i in (0..300).step_by(7) {
        tree.put(&key(i), &[2; 8]).unwrap();
    }
    let minis: Vec<_> = leaves
        .iter()
        .filter(|&&leaf| residency(&tree, leaf) == "mini")
        .collect();
    assert!(!minis.is_empty());
    io.take();
    assert_eq!(tree.iter().count(), 300);
    assert_eq!(io.take().1, Vec::<LeafId>::new(), "no store write");
    assert!(minis.iter().all(|&&leaf| residency(&tree, leaf) == "mini"));
    assert_eq!(tree.check_invariants(), Ok(()));
}
