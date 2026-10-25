use super::*;

fn scanned(tree: &Tree, start: Option<&[u8]>, end: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
    collect(tree.range(start, end).unwrap())
}

/// The records of a sorted vector in `[start, end)`.
fn expected(
    records: &[(Vec<u8>, Vec<u8>)],
    start: Option<&[u8]>,
    end: Option<&[u8]>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    records
        .iter()
        .filter(|(key, _)| start.is_none_or(|start| key.as_slice() >= start))
        .filter(|(key, _)| end.is_none_or(|end| key.as_slice() < end))
        .cloned()
        .collect()
}

/// Every finite leaf fence in the tree: the leaf boundaries.
fn boundaries(tree: &Tree) -> Vec<Vec<u8>> {
    let mut fences: Vec<_> = (0..tree.leaf_count())
        .filter_map(|index| {
            match tree
                .leaf(LeafId::new(index as u64).unwrap())
                .unwrap()
                .fences()?
                .lower
            {
                Fence::Key(key) => Some(key.to_vec()),
                Fence::Unbounded => None,
            }
        })
        .collect();
    fences.sort();
    fences
}

#[test]
fn hand_built_tree_scans_across_leaf_boundaries() {
    let tree = three_level();
    let all: Vec<_> = [&b"a"[..], b"e", b"f", b"lz", b"m", b"s", b"t", b"zz"]
        .iter()
        .map(|key| (key.to_vec(), value_of(key)))
        .collect();
    let bounds: [Option<&[u8]>; 10] = [
        None,
        Some(b""),
        Some(b"a"),
        Some(b"b"),
        Some(b"f"),
        Some(b"m"),
        Some(b"m\0"),
        Some(b"t"),
        Some(b"zz"),
        Some(b"\xff"),
    ];
    for start in bounds {
        for end in bounds {
            if let (Some(start), Some(end)) = (start, end)
                && start > end
            {
                assert_eq!(
                    tree.range(Some(start), Some(end)).err(),
                    Some(TreeError::InvalidRange)
                );
                continue;
            }
            assert_eq!(
                scanned(&tree, start, end),
                expected(&all, start, end),
                "{start:?}..{end:?}"
            );
        }
    }
    assert_eq!(tree.iter().count(), all.len());
}

#[test]
fn grown_tree_scans_match_a_sorted_vector() {
    let mut tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    let mut all = Vec::new();
    for i in (0..900u32).map(|i| (i * 7) % 900) {
        let key = (i * 2).to_be_bytes();
        let value = i.to_le_bytes();
        tree.put(&key, &value).unwrap();
        all.push((key.to_vec(), value.to_vec()));
    }
    all.sort();
    assert!(tree.height() >= 3);
    assert_eq!(scanned(&tree, None, None), all);

    // Bounds at, just inside, and just outside every leaf boundary.
    let fences = boundaries(&tree);
    assert!(fences.len() > 20);
    for (fence, next) in fences.iter().zip(fences.iter().skip(3)) {
        let below = fence[..3].to_vec();
        let after = [fence.as_slice(), b"\0"].concat();
        let starts = [
            Some(fence.as_slice()),
            Some(below.as_slice()),
            Some(after.as_slice()),
            None,
        ];
        let ends = [Some(next.as_slice()), Some(after.as_slice()), None];
        for start in starts {
            for end in ends {
                if start.zip(end).is_some_and(|(start, end)| start > end) {
                    continue;
                }
                assert_eq!(scanned(&tree, start, end), expected(&all, start, end));
            }
        }
        // Equal bounds yield nothing, even at a boundary.
        assert_eq!(tree.range(Some(fence), Some(fence)).unwrap().count(), 0);
    }
}

#[test]
fn scans_skip_empty_leaves() {
    let mut tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    for i in 0..400u32 {
        tree.put(&i.to_be_bytes(), b"v").unwrap();
    }
    // Empty a run of whole leaves in the middle, then the first and last.
    let removed = |i: u32| (100..250).contains(&i) || !(30..370).contains(&i);
    for i in (0..400u32).filter(|&i| removed(i)) {
        tree.remove(&i.to_be_bytes()).unwrap();
    }
    assert_eq!(tree.check_invariants(), Ok(()));
    let empty = (0..tree.leaf_count())
        .filter(|&index| {
            tree.leaf(LeafId::new(index as u64).unwrap())
                .unwrap()
                .is_empty()
        })
        .count();
    assert!(empty >= 3, "{empty} empty leaves");

    let all: Vec<_> = (0..400u32)
        .filter(|&i| !removed(i))
        .map(|i| (i.to_be_bytes().to_vec(), b"v".to_vec()))
        .collect();
    assert_eq!(scanned(&tree, None, None), all);
    let start = 90u32.to_be_bytes();
    let end = 260u32.to_be_bytes();
    assert_eq!(
        scanned(&tree, Some(&start), Some(&end)),
        expected(&all, Some(&start), Some(&end))
    );
    // A range entirely inside the emptied run is empty.
    let start = 120u32.to_be_bytes();
    let end = 240u32.to_be_bytes();
    assert_eq!(tree.range(Some(&start), Some(&end)).unwrap().count(), 0);
}

#[test]
fn scans_are_fused_and_empty_trees_scan_nothing() {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    assert_eq!(tree.iter().count(), 0);

    let tree = two_level();
    let mut scan = tree.range(Some(b"h"), None).unwrap();
    let keys: Vec<_> = scan
        .by_ref()
        .map(|record| record.unwrap().key.to_vec())
        .collect();
    assert_eq!(
        keys,
        [b"h".to_vec(), b"m".to_vec(), b"q".to_vec(), b"z".to_vec()]
    );
    assert!(scan.next().is_none());
    assert!(scan.next().is_none());
}
