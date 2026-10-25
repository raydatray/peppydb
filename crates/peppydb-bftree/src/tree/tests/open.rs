//! The tiling check and the bulk build of inner levels, as pure functions.

use super::*;
use crate::FileStore;
use crate::tree::open::{LeafBounds, build_inner, tile};
use zerocopy::IntoBytes;

fn leaf(raw: u64) -> LeafId {
    LeafId::new(raw).unwrap()
}

fn bounds(id: u64, lower: Option<&[u8]>, upper: Option<&[u8]>) -> LeafBounds {
    LeafBounds {
        id: leaf(id),
        lower: lower.map(<[u8]>::to_vec),
        upper: upper.map(<[u8]>::to_vec),
    }
}

/// `n` leaves over keys `0..n` encoded big-endian, in shuffled ID order.
fn tiling(n: u32) -> Vec<LeafBounds> {
    let key = |i: u32| i.to_be_bytes().to_vec();
    let mut leaves: Vec<_> = (0..n)
        .map(|i| LeafBounds {
            id: leaf(u64::from((i * 7 + 3) % n)),
            lower: (i > 0).then(|| key(i)),
            upper: (i + 1 < n).then(|| key(i + 1)),
        })
        .collect();
    leaves.rotate_left((n / 3) as usize);
    leaves
}

#[test]
fn tile_sorts_a_valid_tiling_by_lower_fence() {
    for n in [1, 2, 3, 10, 257] {
        let sorted = tile(tiling(n)).unwrap();
        assert_eq!(sorted.len(), n as usize);
        assert_eq!(sorted[0].lower, None);
        assert_eq!(sorted[n as usize - 1].upper, None);
        for pair in sorted.windows(2) {
            assert_eq!(pair[0].upper, pair[1].lower);
            assert!(pair[1].lower.is_some());
        }
    }
}

#[test]
fn tile_reports_every_inconsistency() {
    let reason = |leaves: Vec<LeafBounds>| match tile(leaves) {
        Err(TreeError::InconsistentLeaves(reason)) => reason,
        other => panic!("expected inconsistent leaves, got {other:?}"),
    };

    assert_eq!(reason(vec![]), "store holds no leaves");
    assert_eq!(
        reason(vec![bounds(0, Some(b"a"), None)]),
        "no leaf has an unbounded lower fence"
    );
    assert_eq!(
        reason(vec![bounds(0, None, Some(b"m")), bounds(1, None, None)]),
        "two leaves have unbounded lower fences"
    );
    assert_eq!(
        reason(vec![bounds(0, None, None), bounds(1, Some(b"m"), None)]),
        "a leaf before the last has an unbounded upper fence"
    );
    // Gap: [-inf, m) then [n, +inf).
    assert_eq!(
        reason(vec![
            bounds(0, None, Some(b"m")),
            bounds(1, Some(b"n"), None)
        ]),
        "adjacent leaf fences leave a gap or overlap"
    );
    // Overlap: [-inf, n) then [m, +inf).
    assert_eq!(
        reason(vec![
            bounds(0, None, Some(b"n")),
            bounds(1, Some(b"m"), None)
        ]),
        "adjacent leaf fences leave a gap or overlap"
    );
    // Duplicate leaf: [m, t) twice.
    assert_eq!(
        reason(vec![
            bounds(0, None, Some(b"m")),
            bounds(1, Some(b"m"), Some(b"t")),
            bounds(2, Some(b"m"), Some(b"t")),
            bounds(3, Some(b"t"), None),
        ]),
        "adjacent leaf fences leave a gap or overlap"
    );
    assert_eq!(
        reason(vec![bounds(0, None, Some(b"m"))]),
        "the last leaf has a bounded upper fence"
    );
    // A reversed middle leaf whose neighbours happen to line up.
    assert_eq!(
        reason(vec![
            bounds(0, None, Some(b"t")),
            bounds(1, Some(b"t"), Some(b"m")),
            bounds(2, Some(b"m"), None),
        ]),
        "adjacent leaf fences leave a gap or overlap"
    );
}

/// Build a tree over `n` tiling leaves whose pages exist, so that the
/// invariant checker can compare fences with the routing intervals.
fn built(n: u32, inner_capacity: usize) -> Tree {
    let config = config(256, inner_capacity, 8, 16);
    let mut tree = Tree {
        config,
        root: ChildRef::Leaf(leaf(0)),
        inner: Store::new(config.inner_capacity),
        leaves: LeafMap::new(
            Box::new(MemStore::new(Superblock::from_config(&config)).unwrap()),
            [],
        ),
    };
    let leaves = tile(tiling(n)).unwrap();
    for _ in 0..n {
        tree.leaves.alloc().unwrap();
    }
    for bounds in &leaves {
        let fences = Fences {
            lower: bounds.lower.as_deref().map_or(Fence::Unbounded, Fence::Key),
            upper: bounds.upper.as_deref().map_or(Fence::Unbounded, Fence::Key),
        };
        let bytes = tree.leaves.bytes_mut(bounds.id).unwrap();
        let page = Page::init_with_fences(bytes, fences).unwrap();
        if let Some(lower) = &bounds.lower {
            page.try_insert(lower, b"v").unwrap();
        }
    }
    tree.root = build_inner(&mut tree.inner, &leaves).unwrap();
    tree
}

#[test]
fn single_leaf_builds_a_leaf_root() {
    let tree = built(1, 64);
    assert_eq!(tree.root, ChildRef::Leaf(leaf(0)));
    assert_eq!(tree.inner.len(), 0);
    assert_eq!(tree.height(), 1);
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn builds_one_two_and_three_inner_levels() {
    // 64-byte nodes hold (64 - 16) / 16 = 3 four-byte separators, so four
    // children: heights step at 4, 16, and 64 leaves.
    let cases = [(2, 2), (4, 2), (5, 3), (16, 3), (17, 4), (64, 4), (65, 5)];
    for (n, expected_height) in cases {
        let tree = built(n, 64);
        assert_eq!(tree.height(), expected_height, "{n} leaves");
        assert_eq!(tree.check_invariants(), Ok(()), "{n} leaves");
    }

    let tree = built(1000, 4096);
    assert_eq!(tree.height(), 3);
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn routing_from_the_built_tree_matches_a_linear_fence_search() {
    for (n, inner_capacity) in [(5, 64), (40, 64), (300, 128), (1000, 4096)] {
        let tree = built(n, inner_capacity);
        let leaves = tile(tiling(n)).unwrap();
        let expected = |key: &[u8]| {
            leaves
                .iter()
                .rev()
                .find(|leaf| leaf.lower.as_deref().is_none_or(|lower| key >= lower))
                .unwrap()
                .id
        };
        for i in 0..n * 2 + 3 {
            for key in [i.to_be_bytes().to_vec(), [i.to_be_bytes(), [1; 4]].concat()] {
                assert_eq!(
                    tree.find_leaf(&key),
                    expected(&key),
                    "{n} leaves, key {key:?}"
                );
            }
        }
        assert_eq!(tree.find_leaf(b""), leaves[0].id);
        assert_eq!(tree.find_leaf(&[0xff; 8]), leaves.last().unwrap().id);
    }
}

#[test]
fn built_trees_accept_further_writes_and_splits() {
    let mut tree = built(20, 64);
    let height = tree.height();
    for i in 0..200u32 {
        tree.put(&(i * 11).to_be_bytes(), &[2; 16]).unwrap();
    }
    assert!(tree.height() >= height);
    assert_eq!(tree.check_invariants(), Ok(()));
    assert_eq!(
        tree.get(&(11 * 7u32).to_be_bytes()),
        Ok(Some([2; 16].as_slice()))
    );
}

#[test]
fn a_trailing_single_child_borrows_from_its_neighbour() {
    // Four children per node: 5 leaves would leave the last node with one
    // child, so the split becomes 3 + 2.
    let tree = built(5, 64);
    let ChildRef::Inner(root) = tree.root else {
        panic!("root is inner");
    };
    let children: Vec<_> = (0..tree.inner(root).child_count())
        .map(|index| match tree.inner(root).child_at(index).unwrap() {
            ChildRef::Inner(id) => tree.inner(id).child_count(),
            ChildRef::Leaf(_) => panic!("leaves sit below the second level"),
        })
        .collect();
    assert_eq!(children, [3, 2]);
}

// --- Tree::open ---------------------------------------------------------

type Model = std::collections::BTreeMap<Vec<u8>, Vec<u8>>;

fn fill(tree: &mut Tree, model: &mut Model, count: u32) {
    for i in 0..count {
        let key = (i.wrapping_mul(2_654_435_761) % 100_000).to_be_bytes();
        let value = vec![i as u8; (i % 17) as usize];
        tree.put(&key, &value).unwrap();
        model.insert(key.to_vec(), value);
        if i % 5 == 0 {
            let victim = ((i / 2).wrapping_mul(2_654_435_761) % 100_000).to_be_bytes();
            tree.remove(&victim).unwrap();
            model.remove(victim.as_slice());
        }
    }
}

fn assert_matches_model(tree: &Tree, model: &Model) {
    assert_eq!(tree.check_invariants(), Ok(()));
    for (key, value) in model {
        assert_eq!(tree.get(key), Ok(Some(value.as_slice())), "{key:?}");
    }
    let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(collect(tree.iter()), expected);
}

#[test]
fn reopened_memory_trees_match_the_oracle_at_every_height() {
    let config = config(256, 128, 8, 16);
    for count in [0, 3, 60, 400, 3000] {
        let store = MemStore::new(Superblock::from_config(&config)).unwrap();
        let mut tree = Tree::create(config, Box::new(store)).unwrap();
        let mut model = Model::new();
        fill(&mut tree, &mut model, count);
        tree.flush().unwrap();
        let height = tree.height();
        let leaves = tree.leaf_count();

        let reopened = Tree::open(tree.into_store()).unwrap();
        assert_eq!(reopened.config(), &config);
        // Bulk-built levels are packed, so the rebuilt tree is never taller.
        assert!(reopened.height() <= height, "{count} records");
        assert!(reopened.height() >= height.min(3), "{count} records");
        assert_eq!(reopened.leaf_count(), leaves);
        assert_eq!(reopened.resident_leaves(), 0, "open keeps nothing resident");
        assert_eq!(reopened.dirty_leaves(), 0);
        assert_matches_model(&reopened, &model);
        assert_eq!(reopened.resident_leaves(), leaves);
    }
}

#[test]
fn reopened_file_trees_survive_drop_and_keep_accepting_writes() {
    let config = config(512, 256, 16, 64);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.bf");
    let mut model = Model::new();
    let height = {
        let store = FileStore::create(&path, Superblock::from_config(&config)).unwrap();
        let mut tree = Tree::create(config, Box::new(store)).unwrap();
        fill(&mut tree, &mut model, 2000);
        tree.flush().unwrap();
        tree.height()
    };
    assert!(height >= 3);

    let mut tree = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    assert!((3..=height).contains(&tree.height()));
    assert_matches_model(&tree, &model);

    fill(&mut tree, &mut model, 500);
    assert_matches_model(&tree, &model);
    tree.flush().unwrap();
    drop(tree);

    let tree = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    assert_matches_model(&tree, &model);
}

#[test]
fn unflushed_changes_are_lost_and_the_store_stays_consistent() {
    let config = config(256, 128, 8, 16);
    let store = MemStore::new(Superblock::from_config(&config)).unwrap();
    let mut tree = Tree::create(config, Box::new(store)).unwrap();
    let mut model = Model::new();
    fill(&mut tree, &mut model, 100);
    tree.flush().unwrap();
    let mut later = Model::new();
    fill(&mut tree, &mut later, 300);
    assert!(tree.dirty_leaves() > 0);

    let leaves_after = tree.leaf_count();
    let mut reopened = Tree::open(tree.into_store()).unwrap();
    // The store never saw the second batch; it holds the first flush exactly.
    assert_matches_model(&reopened, &model);
    // Slots allocated for the lost batch were never written: they are free,
    // and the next allocations reuse them before the store grows.
    let freed = (0..leaves_after)
        .filter(|&index| !reopened.leaves.is_live(leaf(index as u64)))
        .count();
    assert!(freed > 0);
    assert_eq!(reopened.leaf_count(), leaves_after);
    let mut later = Model::new();
    fill(&mut reopened, &mut later, 300);
    let still_freed = (0..leaves_after)
        .filter(|&index| !reopened.leaves.is_live(leaf(index as u64)))
        .count();
    assert_eq!(still_freed, 0);
    assert_eq!(reopened.check_invariants(), Ok(()));
}

/// A flushed store of a small multi-leaf tree plus the leaf images, for
/// corruption fixtures.
fn flushed_store() -> (Box<dyn PageStore>, Vec<Vec<u8>>) {
    let config = config(256, 128, 8, 16);
    let store = MemStore::new(Superblock::from_config(&config)).unwrap();
    let mut tree = Tree::create(config, Box::new(store)).unwrap();
    let mut model = Model::new();
    fill(&mut tree, &mut model, 200);
    tree.flush().unwrap();
    assert!(tree.leaf_count() >= 4);
    let images = (0..tree.leaf_count())
        .map(|index| tree.leaf(leaf(index as u64)).unwrap().as_bytes().to_vec())
        .collect();
    (tree.into_store(), images)
}

#[test]
fn open_rejects_damaged_leaf_sets_with_errors_not_panics() {
    // A zeroed (freed) middle leaf leaves a gap.
    let (mut store, images) = flushed_store();
    store.free(leaf(1)).unwrap();
    assert_eq!(
        Tree::open(store).err(),
        Some(TreeError::InconsistentLeaves(
            "adjacent leaf fences leave a gap or overlap"
        ))
    );

    // A duplicated leaf: the same page written twice.
    let (mut store, _) = flushed_store();
    store.write(leaf(2), &images[1]).unwrap();
    assert!(matches!(
        Tree::open(store).err(),
        Some(TreeError::InconsistentLeaves(_))
    ));

    // A torn page: the tail of a leaf zeroed.
    let (mut store, _) = flushed_store();
    let mut torn = images[1].clone();
    torn[128..].fill(0);
    store.write(leaf(1), &torn).unwrap();
    assert!(matches!(
        Tree::open(store).err(),
        Some(TreeError::CorruptLeaf { id, .. }) if id == leaf(1)
    ));

    // A flipped header byte.
    let (mut store, _) = flushed_store();
    let mut flipped = images[0].clone();
    flipped[3] ^= 0x40;
    store.write(leaf(0), &flipped).unwrap();
    assert_eq!(
        Tree::open(store).err(),
        Some(TreeError::CorruptLeaf {
            id: leaf(0),
            reason: "capacity field differs from the buffer length",
        })
    );

    // A valid but unfenced page.
    let (mut store, _) = flushed_store();
    let mut unfenced = vec![0; 256];
    Page::init(&mut unfenced).unwrap();
    store.write(leaf(0), &unfenced).unwrap();
    assert_eq!(
        Tree::open(store).err(),
        Some(TreeError::InconsistentLeaves("leaf has no fences"))
    );

    // An empty store, and a store whose only leaf is freed.
    let config = config(256, 128, 8, 16);
    let empty = MemStore::new(Superblock::from_config(&config)).unwrap();
    assert_eq!(
        Tree::open(Box::new(empty)).err(),
        Some(TreeError::InconsistentLeaves("store holds no leaves"))
    );
}

#[test]
fn open_rejects_configurations_the_pages_do_not_respect() {
    // A superblock whose admission rule cannot hold.
    let bad = Superblock {
        page_size: 64,
        inner_capacity: 64,
        max_key_len: 1,
        max_value_len: 0,
    };
    let store = MemStore::new(bad).unwrap();
    assert!(matches!(
        Tree::open(Box::new(store)).err(),
        Some(TreeError::InvalidConfig(_))
    ));

    // Pages written under a wider key limit than the superblock claims.
    let narrow = config(256, 128, 4, 16);
    let mut store = MemStore::new(Superblock::from_config(&narrow)).unwrap();
    let id = store.alloc().unwrap();
    let mut bytes = vec![0; 256];
    let page = Page::init_with_fences(
        &mut bytes,
        Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        },
    )
    .unwrap();
    page.try_insert(&[1; 8], b"v").unwrap();
    store.write(id, &bytes).unwrap();
    assert_eq!(
        Tree::open(Box::new(store)).err(),
        Some(TreeError::InconsistentLeaves(
            "record exceeds the configured key or value length"
        ))
    );

    let mut store = MemStore::new(Superblock::from_config(&narrow)).unwrap();
    for (lower, upper) in [
        (None, Some([7u8; 8].as_slice())),
        (Some([7u8; 8].as_slice()), None),
    ] {
        let id = store.alloc().unwrap();
        let mut bytes = vec![0; 256];
        Page::init_with_fences(
            &mut bytes,
            Fences {
                lower: lower.map_or(Fence::Unbounded, Fence::Key),
                upper: upper.map_or(Fence::Unbounded, Fence::Key),
            },
        )
        .unwrap();
        store.write(id, &bytes).unwrap();
    }
    assert_eq!(
        Tree::open(Box::new(store)).err(),
        Some(TreeError::InconsistentLeaves(
            "fence exceeds the configured key length"
        ))
    );
}
