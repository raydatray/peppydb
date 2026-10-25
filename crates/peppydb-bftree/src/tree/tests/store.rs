//! The leaf mapping table: residency and dirty tracking.

use zerocopy::IntoBytes;

use super::*;

fn leaf(raw: u64) -> LeafId {
    LeafId::new(raw).unwrap()
}

#[test]
fn a_new_tree_has_one_dirty_resident_leaf() {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    assert_eq!(tree.leaf_count(), 1);
    assert_eq!(tree.resident_leaves(), 1);
    assert_eq!(tree.dirty_leaves(), 1);
    assert_eq!(tree.leaves.store().slots(), 1);
}

#[test]
fn reads_do_not_dirty_and_writes_do() {
    let mut tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    for i in 0..200u32 {
        tree.put(&i.to_be_bytes(), b"v").unwrap();
    }
    let leaves = tree.leaf_count();
    assert!(leaves > 2);
    assert_eq!(tree.resident_leaves(), leaves);
    assert_eq!(tree.dirty_leaves(), leaves, "every split output is dirty");
    assert_eq!(tree.leaves.store().slots() as usize, leaves);

    // Copy every page to the store by hand, then map the store afresh:
    // nothing is resident until touched.
    let images: Vec<_> = (0..leaves)
        .map(|index| tree.leaf(leaf(index as u64)).unwrap().as_bytes().to_vec())
        .collect();
    let mut store = tree.leaves.into_store();
    for (index, image) in images.iter().enumerate() {
        store.write(leaf(index as u64), image).unwrap();
    }
    let mut tree = Tree {
        config: tree.config,
        root: tree.root,
        inner: tree.inner,
        leaves: LeafMap::new(store, (0..leaves).map(|_| false)),
    };
    assert_eq!(tree.resident_leaves(), 0);
    assert_eq!(tree.dirty_leaves(), 0);

    let key = 7u32.to_be_bytes();
    assert_eq!(tree.get(&key), Ok(Some(b"v".as_slice())));
    assert_eq!(tree.resident_leaves(), 1);
    assert_eq!(tree.dirty_leaves(), 0);
    assert_eq!(
        tree.check_invariants(),
        Ok(()),
        "the checker loads every leaf"
    );
    assert_eq!(tree.resident_leaves(), leaves);
    assert_eq!(tree.dirty_leaves(), 0);

    tree.put(&key, b"w").unwrap();
    assert_eq!(tree.dirty_leaves(), 1);
}

#[test]
fn unknown_and_freed_leaves_are_rejected_without_panicking() {
    let mut tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    assert_eq!(
        tree.leaf(leaf(5)).err(),
        Some(TreeError::UnknownLeaf(leaf(5)))
    );
    assert_eq!(
        tree.leaf_mut(leaf(5)).err(),
        Some(TreeError::UnknownLeaf(leaf(5)))
    );

    let store = tree.leaves.into_store();
    let tree = Tree {
        config: tree.config,
        root: tree.root,
        inner: tree.inner,
        leaves: LeafMap::new(store, [true]),
    };
    assert_eq!(
        tree.leaf(leaf(0)).err(),
        Some(TreeError::UnknownLeaf(leaf(0)))
    );
    assert_eq!(tree.get(b"a"), Err(TreeError::UnknownLeaf(leaf(0))));
}

#[test]
fn corrupt_stored_leaves_are_reported_on_load() {
    let config = config(256, 128, 8, 16);
    let mut store = MemStore::new(Superblock::from_config(&config)).unwrap();
    let id = store.alloc().unwrap();
    let mut bytes = vec![0; 256];
    Page::init_with_fences(
        &mut bytes,
        Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        },
    )
    .unwrap();
    bytes[2..4].copy_from_slice(&512u16.to_le_bytes());
    store.write(id, &bytes).unwrap();

    let tree = Tree {
        config,
        root: ChildRef::Leaf(id),
        inner: Store::new(config.inner_capacity),
        leaves: LeafMap::new(Box::new(store), [false]),
    };
    assert_eq!(
        tree.get(b"a"),
        Err(TreeError::CorruptLeaf {
            id,
            reason: "capacity field differs from the buffer length",
        })
    );
}

#[test]
fn split_pairs_load_the_left_leaf_on_demand() {
    let mut tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    for i in 0..7u32 {
        tree.put(&i.to_be_bytes(), &[1; 16]).unwrap();
    }
    // Round-trip the full leaf through the store, then force a split.
    let mut page = vec![0; 256];
    page.copy_from_slice(tree.leaf(leaf(0)).unwrap().as_bytes());
    let mut store = tree.leaves.into_store();
    store.write(leaf(0), &page).unwrap();
    let mut tree = Tree {
        config: tree.config,
        root: tree.root,
        inner: tree.inner,
        leaves: LeafMap::new(store, [false]),
    };
    assert_eq!(tree.resident_leaves(), 0);

    tree.put(&7u32.to_be_bytes(), &[1; 16]).unwrap();
    tree.put(&8u32.to_be_bytes(), &[1; 16]).unwrap();
    assert_eq!(tree.height(), 2);
    assert_eq!(tree.leaf_count(), 2);
    assert_eq!(tree.resident_leaves(), 2);
    assert_eq!(tree.dirty_leaves(), 2);
    assert_eq!(tree.check_invariants(), Ok(()));
}
