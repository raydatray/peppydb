//! Full pages: a write-driven merge keeps the merged page in the circular
//! buffer, which then takes writes directly.

use super::store::counting;
use super::*;
use crate::leaf_map::{Locked, Residency};

/// Test configuration with write-load full pages on.
fn config(leaf: usize, inner: usize, key: usize, value: usize) -> TreeConfig {
    TreeConfig {
        write_load_full_page: true,
        ..super::config(leaf, inner, key, value)
    }
}

fn leaf0() -> LeafId {
    LeafId::new(0).unwrap()
}

fn is_full(tree: &Tree, id: LeafId) -> bool {
    matches!(
        tree.leaves.read(id).unwrap().residency(),
        Residency::Full(_)
    )
}

fn is_dirty(tree: &Tree, id: LeafId) -> bool {
    tree.leaves.write(id).unwrap().is_dirty()
}

/// Fill leaf 0's mini-page until a write needs a merge; that merge keeps the
/// page. Keys are single bytes, so they all stay in leaf 0 if it fits.
fn promote_by_writes(tree: &Tree) -> u8 {
    let mut i = 0u8;
    while !is_full(tree, leaf0()) {
        tree.put(&[i], &[i; 8]).unwrap();
        i += 1;
        assert!(i < 60, "a merge happened");
    }
    i
}

#[test]
fn a_write_driven_merge_keeps_the_page_and_later_writes_go_in_directly() {
    let tree = Tree::in_memory(config(512, 256, 8, 16)).unwrap();
    let n = promote_by_writes(&tree);
    assert_eq!(tree.leaf_count(), 1);
    assert_eq!(tree.buffered_leaves(), 0);
    for i in 0..n {
        assert_eq!(tree.get(&[i]), Ok(Some(vec![i; 8])));
    }

    // Direct writes: Insert upserts, Delete removes; both dirty the page.
    tree.put(&[200], b"direct").unwrap();
    assert!(is_full(&tree, leaf0()));
    assert!(is_dirty(&tree, leaf0()));
    tree.remove(&[0]).unwrap();
    assert_eq!(tree.get(&[0]), Ok(None));
    assert_eq!(tree.get(&[200]), Ok(Some(b"direct".to_vec())));
    assert_eq!(tree.buffered_leaves(), 0, "nothing was buffered");
    assert_eq!(tree.check_invariants(), Ok(()));

    // Scans read the full page.
    assert_eq!(tree.iter().count(), usize::from(n));
}

#[test]
fn a_full_page_that_overflows_is_written_back_released_and_buffered() {
    let config = config(512, 256, 8, 16);
    let (store, io) = counting(config);
    let tree = Tree::create(config, store).unwrap();
    promote_by_writes(&tree);
    io.take();
    // Fill the full page with direct writes until one overflows.
    let mut i = 100u8;
    while is_full(&tree, leaf0()) {
        tree.put(&[i], &[i; 16]).unwrap();
        i += 1;
    }
    let (_, writes) = io.take();
    assert_eq!(
        writes,
        vec![leaf0()],
        "the dirty page was written back once"
    );
    assert_eq!(
        tree.buffered_leaves(),
        1,
        "the overflowing record was buffered"
    );
    assert_eq!(tree.get(&[i - 1]), Ok(Some(vec![i - 1; 16])));
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn eviction_writes_back_dirty_full_pages_and_nothing_for_clean_ones() {
    let config = config(512, 256, 8, 16);
    let (store, io) = counting(config);
    let tree = Tree::create(config, store).unwrap();
    promote_by_writes(&tree);
    // The record that triggered the merge went straight into the page, so
    // it starts dirty; a flush writes it back and keeps it.
    assert!(is_dirty(&tree, leaf0()));
    tree.checkpoint().unwrap();
    io.take();

    // Clean: eviction writes nothing.
    assert!(!is_dirty(&tree, leaf0()));
    assert_eq!(tree.evict_one(), Ok(crate::evict::Eviction::Evicted));
    assert!(!is_full(&tree, leaf0()));
    assert_eq!(io.take().1, Vec::<LeafId>::new());

    // Dirty: promote again, write directly, evict: one write-back.
    for i in 60..120u8 {
        tree.put(&[i], &[i; 8]).unwrap();
        if is_full(&tree, leaf0()) {
            break;
        }
    }
    assert!(is_full(&tree, leaf0()));
    // Key [1] stays in leaf 0 even if the promoting merge split the leaf.
    tree.put(&[1], b"dirty").unwrap();
    assert!(is_dirty(&tree, leaf0()));
    io.take();
    assert_eq!(tree.evict_one(), Ok(crate::evict::Eviction::Evicted));
    assert_eq!(io.take().1, vec![leaf0()]);
    assert!(!is_full(&tree, leaf0()));
    assert_eq!(tree.get(&[1]), Ok(Some(b"dirty".to_vec())));
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn flush_writes_back_dirty_full_pages_and_reopen_agrees() {
    let tree = Tree::in_memory(config(512, 256, 8, 16)).unwrap();
    let n = promote_by_writes(&tree);
    tree.put(&[222], b"direct").unwrap();
    tree.checkpoint().unwrap();
    assert!(!is_dirty(&tree, leaf0()), "flush cleaned the page");
    assert!(is_full(&tree, leaf0()), "and kept it");
    let reopened = Tree::open(tree.into_store()).unwrap();
    assert_eq!(reopened.get(&[222]), Ok(Some(b"direct".to_vec())));
    assert_eq!(reopened.iter().count(), usize::from(n) + 1);
}

#[test]
fn with_the_knob_off_merges_leave_the_leaf_base() {
    let config = TreeConfig {
        write_load_full_page: false,
        ..config(512, 256, 8, 16)
    };
    assert!(!config.write_load_full_page);
    let tree = Tree::in_memory(config).unwrap();
    for i in 0..60u8 {
        tree.put(&[i], &[i; 8]).unwrap();
        assert!(!is_full(&tree, leaf0()));
    }
}

#[test]
fn relocation_copies_a_full_page_and_keeps_it_dirty() {
    let config = TreeConfig {
        second_chance_percent: 100,
        ..config(512, 256, 8, 16)
    };
    let tree = Tree::in_memory(config).unwrap();
    promote_by_writes(&tree);
    tree.put(&[230], b"dirty").unwrap();
    let before = tree.leaves.read(leaf0()).unwrap().block_addr();
    tree.get(&[230]).unwrap();
    let after = tree.leaves.read(leaf0()).unwrap().block_addr();
    assert!(after > before, "the read relocated the full page");
    assert!(is_full(&tree, leaf0()) && is_dirty(&tree, leaf0()));
    assert_eq!(tree.get(&[230]), Ok(Some(b"dirty".to_vec())));
}

#[test]
fn the_entry_stays_within_a_cache_line() {
    assert!(size_of::<bf_tree_sync::RwLock<crate::leaf_map::LeafEntry>>() <= 64);
}
