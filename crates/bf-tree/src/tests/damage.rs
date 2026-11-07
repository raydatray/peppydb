//! Damaged and inconsistent stored pages, on both backends (plan 0017).

use super::*;
use bf_tree_store::FileStore;

fn leaf(raw: u64) -> LeafId {
    LeafId::new(raw).unwrap()
}

fn small() -> TreeConfig {
    config(256, 128, 8, 16)
}

/// A flushed multi-leaf tree on a fresh store of each backend. The
/// directory keeps a file store's path alive.
fn flushed(file: bool) -> (Tree, Option<tempfile::TempDir>) {
    let config = small();
    let superblock = Superblock::from(&config);
    let (store, dir): (Box<dyn PageStore>, _) = if file {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::create(dir.path().join("damage.bf"), superblock).unwrap();
        (Box::new(store), Some(dir))
    } else {
        (Box::new(MemStore::new(superblock).unwrap()), None)
    };
    let tree = Tree::create(config, store).unwrap();
    for i in 0..300u32 {
        tree.put(&i.to_be_bytes(), &[i as u8; 12]).unwrap();
    }
    tree.checkpoint().unwrap();
    assert!(tree.leaf_count() >= 4);
    (tree, dir)
}

/// Read, change, and write back leaf `id`'s stored page.
fn damage(tree: &Tree, id: LeafId, change: impl FnOnce(&mut Vec<u8>)) {
    let page_number = tree.leaves.page_of(id).unwrap();
    let store = tree.store();
    let mut bytes = vec![0; store.page_size()];
    store.read(page_number, &mut bytes).unwrap();
    change(&mut bytes);
    store.write(page_number, &bytes).unwrap();
}

/// A key in leaf `id`'s range.
fn key_in(tree: &Tree, id: LeafId) -> Vec<u8> {
    (0..300u32)
        .map(|i| i.to_be_bytes().to_vec())
        .find(|key| tree.find_leaf(key) == id)
        .unwrap()
}

#[test]
fn a_flipped_byte_fails_the_load_and_the_open() {
    for file in [false, true] {
        let (tree, _dir) = flushed(file);
        let victim = leaf(1);
        let page_number = tree.leaves.page_of(victim).unwrap();
        let key = key_in(&tree, victim);
        damage(&tree, victim, |bytes| bytes[100] ^= 0x01);
        assert_eq!(
            tree.get(&key),
            Err(TreeError::CorruptLeaf {
                id: victim,
                reason: "checksum mismatch",
            }),
            "file {file}"
        );
        // Other leaves still answer.
        assert!(tree.get(&key_in(&tree, leaf(0))).unwrap().is_some());
        assert_eq!(
            Tree::open(tree.into_store()).err(),
            Some(TreeError::CorruptPage {
                page_number,
                reason: "checksum mismatch",
            }),
            "file {file}"
        );
    }
}

#[test]
fn a_page_naming_another_leaf_fails_the_load() {
    for file in [false, true] {
        let (tree, _dir) = flushed(file);
        let key = key_in(&tree, leaf(2));
        damage(&tree, leaf(2), |bytes| stamp(bytes, 3));
        assert_eq!(
            tree.get(&key),
            Err(TreeError::CorruptLeaf {
                id: leaf(2),
                reason: "names another leaf",
            })
        );
        // On open, leaf 3 is then claimed twice.
        assert_eq!(
            Tree::open(tree.into_store()).err(),
            Some(TreeError::InconsistentLeaves(
                "two stored pages name the same leaf"
            ))
        );
    }
}

#[test]
fn a_stored_page_without_a_leaf_or_with_a_huge_leaf_id_fails_the_open() {
    let (tree, _dir) = flushed(false);
    damage(&tree, leaf(1), |bytes| {
        let page: &mut Page = zerocopy::FromBytes::mut_from_bytes(&mut bytes[..]).unwrap();
        page.set_leaf(None).unwrap();
        page.seal();
    });
    assert_eq!(
        Tree::open(tree.into_store()).err(),
        Some(TreeError::InconsistentLeaves("a stored page names no leaf"))
    );

    let (tree, _dir) = flushed(false);
    // The largest ID a stamp holds: open rejects it before sizing tables.
    damage(&tree, leaf(1), |bytes| {
        stamp(bytes, u64::from(u32::MAX) - 1)
    });
    assert_eq!(
        Tree::open(tree.into_store()).err(),
        Some(TreeError::InconsistentLeaves(
            "a stored page names an implausibly large leaf ID"
        ))
    );
}

#[test]
fn holes_stay_unused_and_zero_pages_are_reused_after_open() {
    for file in [false, true] {
        let (tree, dir) = flushed(file);
        // A leaf allocated but never written leaves a hole and no page; an
        // allocated page that was never written is all zeros.
        drop(tree.leaves.alloc().unwrap());
        let zero = tree.store().alloc().unwrap();
        let leaves = tree.leaf_count();
        let pages = tree.store().page_count();
        let store = tree.into_store();
        let store: Box<dyn PageStore> = match &dir {
            Some(dir) => {
                drop(store);
                Box::new(FileStore::open(dir.path().join("damage.bf")).unwrap())
            }
            None => store,
        };
        let tree = Tree::open(store).unwrap();
        assert_eq!(tree.leaf_count(), leaves - 1, "the trailing hole is gone");
        assert_eq!(tree.check_invariants(), Ok(()));
        assert_eq!(tree.store().alloc().unwrap(), zero, "the zero page is free");
        assert_eq!(tree.store().page_count(), pages);
        // New leaves get IDs after every existing one.
        let fresh = tree.leaves.alloc().unwrap();
        assert_eq!(fresh.id().index(), tree.leaves.len() - 1);
    }
}

#[test]
fn a_hole_below_the_largest_leaf_stays_freed_after_open() {
    let (tree, _dir) = flushed(false);
    // Discarding a written leaf leaves a hole below later leaves.
    let right = tree.leaves.alloc().unwrap();
    let id = right.id();
    let mut bytes = vec![0; 256];
    Page::init_with_fences(
        &mut bytes,
        Fences {
            lower: Fence::Key(b"zz"),
            upper: Fence::Unbounded,
        },
    )
    .unwrap();
    tree.leaves.write_through(&mut [(id, &mut bytes)]).unwrap();
    tree.leaves.discard(right).unwrap();
    // Later splits take IDs above the hole; holes are never reused.
    let leaves = tree.leaf_count();
    for i in 300..600u32 {
        tree.put(&i.to_be_bytes(), &[i as u8; 12]).unwrap();
    }
    tree.checkpoint().unwrap();
    assert!(tree.leaf_count() > leaves);
    assert!(!tree.leaves.is_live(id));
    let tree = Tree::open(tree.into_store()).unwrap();
    assert!(
        tree.leaves.len() > id.index() + 1,
        "the hole is below the largest leaf"
    );
    assert!(!tree.leaves.is_live(id), "the discarded leaf is a hole");
    assert_eq!(tree.check_invariants(), Ok(()));
    for i in 0..600u32 {
        assert_eq!(tree.get(&i.to_be_bytes()), Ok(Some(vec![i as u8; 12])));
    }
}
