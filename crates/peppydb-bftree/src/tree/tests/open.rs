//! The tiling check and the bulk build of inner levels, as pure functions.

use super::*;
use crate::tree::open::{LeafBounds, build_inner, tile};

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
