use super::*;
use crate::tree::invariants::InvariantError;

fn leaf(raw: u64) -> LeafId {
    LeafId::new(raw).unwrap()
}

fn inner(raw: u64) -> InnerId {
    InnerId::new(raw).unwrap()
}

#[test]
fn single_leaf_tree_serves_lookups() {
    let mut tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    assert_eq!(tree.height(), 1);
    assert_eq!(tree.get(b"a"), Ok(None));
    assert_eq!(tree.locate(b"a"), (leaf(0), vec![]));
    let root = tree.find_leaf(b"");
    tree.leaf_mut(root).unwrap().try_insert(b"a", b"1").unwrap();
    assert_eq!(tree.get(b"a"), Ok(Some(b"1".as_slice())));
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn keys_rejected_by_the_configuration() {
    let tree = two_level();
    assert_eq!(tree.get(b""), Err(TreeError::EmptyKey));
    assert_eq!(tree.get(&[b'a'; 9]), Err(TreeError::KeyTooLarge));
    assert_eq!(tree.get(&[b'a'; 8]), Ok(None));
}

#[test]
fn two_level_routes_at_and_between_separators() {
    let tree = two_level();
    assert_eq!(tree.height(), 2);
    assert_eq!(tree.check_invariants(), Ok(()));
    let cases: [(&[u8], u64, usize); 9] = [
        (b"", 0, 0),
        (b"a", 0, 0),
        (b"ez", 0, 0),
        (b"f", 1, 1),
        (b"f\0", 1, 1),
        (b"lzzz", 1, 1),
        (b"m", 2, 2),
        (b"q", 2, 2),
        (b"\xff", 2, 2),
    ];
    for (key, expected_leaf, index) in cases {
        assert_eq!(
            tree.locate(key),
            (leaf(expected_leaf), vec![(inner(0), index)]),
            "{key:?}"
        );
        assert_eq!(tree.find_leaf(key), leaf(expected_leaf));
    }
    for key in [&b"a"[..], b"c", b"e", b"f", b"h", b"m", b"q", b"z"] {
        assert_eq!(tree.get(key), Ok(Some(value_of(key).as_slice())), "{key:?}");
    }
    for key in [&b"b"[..], b"d", b"g", b"l", b"n", b"zz"] {
        assert_eq!(tree.get(key), Ok(None), "{key:?}");
    }
}

#[test]
fn three_level_records_the_full_path() {
    let tree = three_level();
    assert_eq!(tree.height(), 3);
    assert_eq!(tree.check_invariants(), Ok(()));
    // Inner IDs: 0 = left, 1 = right, 2 = root.
    type Case = (&'static [u8], u64, [(u64, usize); 2]);
    let cases: [Case; 6] = [
        (b"a", 0, [(2, 0), (0, 0)]),
        (b"f", 1, [(2, 0), (0, 1)]),
        (b"lz", 1, [(2, 0), (0, 1)]),
        (b"m", 2, [(2, 1), (1, 0)]),
        (b"t", 3, [(2, 1), (1, 1)]),
        (b"zzz", 3, [(2, 1), (1, 1)]),
    ];
    for (key, expected_leaf, path) in cases {
        let path: Vec<_> = path.iter().map(|&(id, index)| (inner(id), index)).collect();
        assert_eq!(tree.locate(key), (leaf(expected_leaf), path), "{key:?}");
    }
    for key in [&b"a"[..], b"e", b"f", b"lz", b"m", b"s", b"t", b"zz"] {
        assert_eq!(tree.get(key), Ok(Some(value_of(key).as_slice())), "{key:?}");
    }
    assert_eq!(tree.get(b"n"), Ok(None));
}

#[test]
fn checker_catches_a_mismatched_fence() {
    let mut tree = empty_tree();
    let l0 = add_leaf(&mut tree, None, Some(b"f"), &[]);
    // The parent routes `[g, +inf)` here, but the leaf claims `[f, +inf)`.
    let l1 = add_leaf(&mut tree, Some(b"f"), None, &[]);
    tree.root = add_inner(&mut tree, l0, &[(b"g", l1)]);
    assert_eq!(
        tree.check_invariants(),
        Err(InvariantError::FenceMismatch(leaf(0)))
    );

    // An unfenced leaf never matches its interval.
    let mut tree = empty_tree();
    let id = tree.leaves.alloc().unwrap();
    Page::init(tree.leaves.bytes_mut(id).unwrap()).unwrap();
    tree.root = ChildRef::Leaf(id);
    assert_eq!(
        tree.check_invariants(),
        Err(InvariantError::FenceMismatch(id))
    );
}

#[test]
fn checker_catches_uneven_depth() {
    // The right subtree has one more level than the left.
    let mut tree = empty_tree();
    let l0 = add_leaf(&mut tree, None, Some(b"f"), &[]);
    let l1 = add_leaf(&mut tree, Some(b"f"), Some(b"m"), &[]);
    let l2 = add_leaf(&mut tree, Some(b"m"), Some(b"p"), &[]);
    let l3 = add_leaf(&mut tree, Some(b"p"), Some(b"s"), &[]);
    let l4 = add_leaf(&mut tree, Some(b"s"), Some(b"t"), &[]);
    let l5 = add_leaf(&mut tree, Some(b"t"), None, &[]);
    let left = add_inner(&mut tree, l0, &[(b"f", l1)]);
    let x = add_inner(&mut tree, l2, &[(b"p", l3)]);
    let z = add_inner(&mut tree, l4, &[(b"t", l5)]);
    let right = add_inner(&mut tree, x, &[(b"s", z)]);
    tree.root = add_inner(&mut tree, left, &[(b"m", right)]);
    assert_eq!(
        tree.check_invariants(),
        Err(InvariantError::UnevenDepth(leaf(2)))
    );
}

#[test]
fn checker_catches_unreachable_duplicate_and_dangling_nodes() {
    let mut tree = two_level();
    tree.leaves.alloc().unwrap();
    assert_eq!(
        tree.check_invariants(),
        Err(InvariantError::Unreachable(ChildRef::Leaf(leaf(3))))
    );

    let mut tree = two_level();
    let orphan = add_inner(
        &mut tree,
        ChildRef::Leaf(leaf(0)),
        &[(b"b", ChildRef::Leaf(leaf(1)))],
    );
    assert_eq!(
        tree.check_invariants(),
        Err(InvariantError::Unreachable(orphan))
    );

    let mut tree = empty_tree();
    let l0 = add_leaf(&mut tree, None, Some(b"f"), &[]);
    tree.root = add_inner(&mut tree, l0, &[(b"f", l0)]);
    assert_eq!(tree.check_invariants(), Err(InvariantError::Duplicate(l0)));

    let mut tree = empty_tree();
    let l0 = add_leaf(&mut tree, None, Some(b"f"), &[]);
    let missing = ChildRef::Leaf(leaf(7));
    tree.root = add_inner(&mut tree, l0, &[(b"f", missing)]);
    assert_eq!(
        tree.check_invariants(),
        Err(InvariantError::Dangling(missing))
    );
}

#[test]
fn checker_catches_empty_inner_nodes_and_stray_records() {
    let mut tree = empty_tree();
    let l0 = add_leaf(&mut tree, None, None, &[]);
    tree.root = add_inner(&mut tree, l0, &[]);
    assert_eq!(
        tree.check_invariants(),
        Err(InvariantError::EmptyInner(inner(0)))
    );

    // Rewrite the high fence of `[a, z)` to `x`, stranding record `y`.
    let mut tree = empty_tree();
    let l0 = add_leaf(&mut tree, None, Some(b"a"), &[]);
    let l1 = add_leaf(&mut tree, Some(b"a"), Some(b"z"), &[b"y"]);
    let l2 = add_leaf(&mut tree, Some(b"x"), None, &[]);
    tree.root = add_inner(&mut tree, l0, &[(b"a", l1), (b"x", l2)]);
    let bytes = tree.leaves.bytes_mut(leaf(1)).unwrap();
    let offset = usize::from(u16::from_le_bytes([bytes[40], bytes[41]]));
    assert_eq!(bytes[offset], b'z');
    bytes[offset] = b'x';
    assert_eq!(
        tree.check_invariants(),
        Err(InvariantError::RecordOutOfRange(leaf(1)))
    );
}
