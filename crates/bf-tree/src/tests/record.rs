//! Record caching: base-served reads leave cached records and absences.

use super::store::{counting, stored_pages};
use super::*;
use crate::evict::Eviction;
use crate::leaf_map::{Locked, Residency};
use crate::maintain::Hint;

/// Record mode: every base-served read caches.
fn config() -> TreeConfig {
    TreeConfig {
        read_promotion_percent: 100,
        read_record_cache: true,
        ..super::config(512, 256, 8, 16)
    }
}

fn key(i: u32) -> [u8; 4] {
    i.to_be_bytes()
}

fn is_mini(tree: &Tree, id: LeafId) -> bool {
    matches!(
        tree.leaves.read(id).unwrap().residency(),
        Residency::Mini { .. }
    )
}

/// A tree over a counting store with `n` flushed keys and an empty
/// circular buffer.
fn flushed(config: TreeConfig, n: u32) -> (Tree, super::store::Io) {
    let (store, io) = counting(config);
    let tree = Tree::create(config, store).unwrap();
    for i in 0..n {
        tree.put(&key(i * 2), b"v").unwrap();
    }
    tree.checkpoint().unwrap();
    while tree.evict_one() == Ok(Eviction::Evicted) {}
    io.take();
    (tree, io)
}

#[test]
fn the_second_read_of_a_present_or_absent_key_reads_no_base_page() {
    let (tree, io) = flushed(config(), 10);
    assert_eq!(tree.get(&key(4)), Ok(Some(b"v".to_vec())));
    assert_eq!(tree.get(&key(5)), Ok(None));
    assert_eq!(io.take(), (2, vec![]));
    assert_eq!(tree.get(&key(4)), Ok(Some(b"v".to_vec())));
    assert_eq!(tree.get(&key(5)), Ok(None));
    assert_eq!(
        io.take(),
        (0, vec![]),
        "the record and the phantom answered"
    );
    // Another key still needs the base page.
    assert_eq!(tree.get(&key(6)), Ok(Some(b"v".to_vec())));
    assert_eq!(io.take().0, 1);
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn rate_zero_caches_nothing() {
    let (tree, io) = flushed(
        TreeConfig {
            read_promotion_percent: 0,
            ..config()
        },
        10,
    );
    for _ in 0..3 {
        assert_eq!(tree.get(&key(4)), Ok(Some(b"v".to_vec())));
    }
    assert_eq!(io.take().0, 3);
    assert_eq!(tree.buffered_leaves(), 0);
}

#[test]
fn a_fill_never_hides_a_newer_buffered_write() {
    let (tree, _io) = flushed(config(), 10);
    let leaf = tree.find_leaf(&key(4));
    tree.put(&key(8), b"w").unwrap(); // the leaf now has a mini-page
    let noted = tree
        .note(&tree.leaves.read(leaf).unwrap())
        .with_hint(Some(Hint::Cache {
            key: key(4).to_vec(),
            value: Some(b"v".to_vec()),
        }));
    // A buffered write bumps the version, so `maintain` drops the stale
    // fill; `SkippedDirty` would stop it otherwise.
    tree.put(&key(4), b"new").unwrap();
    tree.maintain(noted).unwrap();
    assert_eq!(tree.get(&key(4)), Ok(Some(b"new".to_vec())));
    // So is a phantom over a buffered insert.
    let noted = tree
        .note(&tree.leaves.read(leaf).unwrap())
        .with_hint(Some(Hint::Cache {
            key: key(9).to_vec(),
            value: None,
        }));
    tree.put(&key(9), b"x").unwrap();
    tree.maintain(noted).unwrap();
    assert_eq!(tree.get(&key(9)), Ok(Some(b"x".to_vec())));
    tree.checkpoint().unwrap();
    assert_eq!(tree.get(&key(4)), Ok(Some(b"new".to_vec())));
}

#[test]
fn a_moving_working_set_keeps_being_cached_without_writes() {
    let config = TreeConfig {
        circular_buffer_bytes: 2048,
        ..config()
    };
    let (tree, io) = flushed(config, 3000);
    // One key per leaf, many more leaves than the buffer holds mini-pages.
    let mut keys: Vec<_> = (0..3000)
        .map(|i| (tree.find_leaf(&key(i * 2)), i * 2))
        .collect();
    keys.dedup_by_key(|(leaf, _)| *leaf);
    assert!(keys.len() > 10, "{} leaves", keys.len());
    for window in keys.chunks(2) {
        for (_, k) in window {
            assert_eq!(tree.get(&key(*k)), Ok(Some(b"v".to_vec())));
        }
        assert_eq!(io.take().0, window.len(), "first reads load the base");
        for (leaf, k) in window {
            assert_eq!(tree.get(&key(*k)), Ok(Some(b"v".to_vec())));
            assert!(is_mini(&tree, *leaf));
        }
        assert_eq!(io.take(), (0, vec![]), "second reads hit the cache");
    }
    let cached = keys
        .iter()
        .filter(|(leaf, _)| is_mini(&tree, *leaf))
        .count();
    assert!(
        cached < keys.len(),
        "older cached records were freed: {cached} of {}",
        keys.len()
    );
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn growth_drops_unreferenced_clean_records_and_keeps_the_rest() {
    let (tree, io) = flushed(config(), 10);
    let leaf = tree.find_leaf(&key(0));
    // Cache a record, a phantom, then the record that is hit. A fill can
    // itself grow the mini-page, so the hit comes last.
    for k in [2, 3, 0] {
        tree.get(&key(k)).unwrap();
    }
    io.take();
    tree.get(&key(0)).unwrap();
    assert_eq!(io.take().0, 0, "the hit marks the record's bit");
    // Dirty writes until the mini-page grows into a new block.
    let before = tree.leaves.read(leaf).unwrap().block_addr();
    let mut i = 100;
    while tree.leaves.read(leaf).unwrap().block_addr() == before {
        tree.put(&key(i), &[0; 16]).unwrap();
        i += 1;
        assert!(i < 200 && is_mini(&tree, leaf));
    }
    assert_eq!(io.take(), (0, vec![]));
    assert_eq!(tree.get(&key(0)), Ok(Some(b"v".to_vec())));
    assert_eq!(io.take().0, 0, "the referenced record survived");
    assert_eq!(tree.get(&key(100)), Ok(Some(vec![0; 16])));
    assert_eq!(io.take().0, 0, "dirty records survived");
    assert_eq!(tree.get(&key(2)), Ok(Some(b"v".to_vec())));
    assert_eq!(tree.get(&key(3)), Ok(None));
    assert_eq!(io.take().0, 2, "the cold record and phantom were dropped");
}

#[test]
fn cache_only_mini_pages_are_freed_by_flush_and_eviction_without_writes() {
    let (tree, io) = flushed(config(), 10);
    let stored = stored_pages(&tree);
    tree.get(&key(2)).unwrap();
    tree.get(&key(3)).unwrap();
    assert_eq!(tree.buffered_leaves(), 1);
    io.take();
    tree.checkpoint().unwrap();
    assert_eq!(io.take(), (0, vec![]), "flush wrote nothing");
    assert_eq!(tree.buffered_leaves(), 0);

    tree.get(&key(2)).unwrap();
    io.take();
    assert_eq!(tree.evict_one(), Ok(Eviction::Evicted));
    assert_eq!(io.take(), (0, vec![]), "eviction wrote nothing");
    assert_eq!(tree.buffered_leaves(), 0);
    assert_eq!(
        stored_pages(&tree),
        stored,
        "cached records never reached the store"
    );
}
