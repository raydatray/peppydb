use std::ops::ControlFlow;

use super::*;

/// Owned key-value pairs in key order.
type Records = Vec<(Vec<u8>, Vec<u8>)>;

/// A scan's start and end bounds.
type Bounds<'a> = (Option<&'a [u8]>, Option<&'a [u8]>);

fn scanned(tree: &Tree, start: Option<&[u8]>, end: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
    collect(tree.range(start, end).unwrap())
}

/// The records `scan_with` visits in `[start, end)`, copied.
fn visited(tree: &Tree, start: Option<&[u8]>, end: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut records = Vec::new();
    tree.scan_with(start, end, |key, value| {
        records.push((key.to_vec(), value.to_vec()));
        ControlFlow::Continue(())
    })
    .unwrap();
    records
}

/// A tree with tiny nodes holding the even keys `0, 2, .., 2 * (count - 1)`.
fn grown(count: u32) -> (Tree, Records) {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    let mut all = Vec::new();
    for i in (0..count).map(|i| (i * 7) % count) {
        let key = (i * 2).to_be_bytes();
        let value = i.to_le_bytes();
        tree.put(&key, &value).unwrap();
        all.push((key.to_vec(), value.to_vec()));
    }
    all.sort();
    (tree, all)
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
            let bytes = tree
                .leaves
                .page_copy(LeafId::new(index as u64).unwrap())
                .unwrap();
            match Page::validate(&bytes).unwrap().fences()?.lower {
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
    let (tree, all) = grown(900);
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
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    for i in 0..400u32 {
        tree.put(&i.to_be_bytes(), b"v").unwrap();
    }
    tree.flush().unwrap();
    // Empty a run of whole leaves in the middle, then the first and last.
    let removed = |i: u32| (100..250).contains(&i) || !(30..370).contains(&i);
    for i in (0..400u32).filter(|&i| removed(i)) {
        tree.remove(&i.to_be_bytes()).unwrap();
    }
    assert_eq!(tree.check_invariants(), Ok(()));
    let all: Vec<_> = (0..400u32)
        .filter(|&i| !removed(i))
        .map(|i| (i.to_be_bytes().to_vec(), b"v".to_vec()))
        .collect();

    // Scan with tombstones still buffered over the base records, then again
    // after the merges have emptied the base pages.
    for flushed in [false, true] {
        if flushed {
            tree.flush().unwrap();
            assert_eq!(tree.check_invariants(), Ok(()));
            assert!(
                empty_leaves(&tree) >= 3,
                "{} empty leaves",
                empty_leaves(&tree)
            );
        }
        assert_eq!(scanned(&tree, None, None), all, "flushed: {flushed}");
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
}

/// Number of leaves whose base page holds no record.
fn empty_leaves(tree: &Tree) -> usize {
    (0..tree.leaf_count())
        .filter(|&index| {
            let bytes = tree
                .leaves
                .page_copy(LeafId::new(index as u64).unwrap())
                .unwrap();
            Page::validate(&bytes).unwrap().is_empty()
        })
        .count()
}

#[test]
fn scans_are_fused_and_empty_trees_scan_nothing() {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    assert_eq!(tree.iter().count(), 0);

    let tree = two_level();
    let mut scan = tree.range(Some(b"h"), None).unwrap();
    let keys: Vec<_> = scan.by_ref().map(|record| record.unwrap().0).collect();
    assert_eq!(
        keys,
        [b"h".to_vec(), b"m".to_vec(), b"q".to_vec(), b"z".to_vec()]
    );
    assert!(scan.next().is_none());
    assert!(scan.next().is_none());
}

#[test]
fn scan_with_visits_the_same_records_as_range() {
    let tree = three_level();
    let bounds: [Option<&[u8]>; 7] = [
        None,
        Some(b""),
        Some(b"b"),
        Some(b"f"),
        Some(b"m\0"),
        Some(b"zz"),
        Some(b"\xff"),
    ];
    for start in bounds {
        for end in bounds {
            if start.zip(end).is_some_and(|(start, end)| start > end) {
                assert_eq!(
                    tree.scan_with(start, end, |_, _| ControlFlow::Continue(())),
                    Err(TreeError::InvalidRange)
                );
                continue;
            }
            assert_eq!(
                visited(&tree, start, end),
                scanned(&tree, start, end),
                "{start:?}..{end:?}"
            );
        }
    }

    let (tree, all) = grown(900);
    assert!(tree.height() >= 3);
    assert_eq!(visited(&tree, None, None), all);
    assert_eq!(visited(&tree, None, None), scanned(&tree, None, None));

    // Empty ranges: equal bounds, and a gap between two adjacent keys.
    let key = 100u32.to_be_bytes();
    assert_eq!(visited(&tree, Some(&key), Some(&key)), []);
    let gap = (101u32.to_be_bytes(), 102u32.to_be_bytes());
    assert_eq!(visited(&tree, Some(&gap.0), Some(&gap.1)), []);
    assert_eq!(scanned(&tree, Some(&gap.0), Some(&gap.1)), []);

    // Ranges that span many leaves, with bounds at and between fences.
    let fences = boundaries(&tree);
    assert!(fences.len() > 20);
    for (fence, next) in fences.iter().zip(fences.iter().skip(10)) {
        let after = [fence.as_slice(), b"\0"].concat();
        for start in [Some(fence.as_slice()), Some(after.as_slice()), None] {
            for end in [Some(next.as_slice()), None] {
                let records = visited(&tree, start, end);
                assert_eq!(records, scanned(&tree, start, end));
                assert_eq!(records, expected(&all, start, end));
            }
        }
    }
}

#[test]
fn scan_with_stops_at_the_first_break() {
    let (tree, all) = grown(900);
    let start = 200u32.to_be_bytes();
    let in_range = expected(&all, Some(&start), None);
    // Stop in the first leaf, at its end, and many leaves later.
    for limit in [1, 2, 5, 100, 700] {
        let mut visits = 0;
        let mut keys = Vec::new();
        let result = tree.scan_with(Some(&start), None, |key, _| {
            visits += 1;
            keys.push(key.to_vec());
            if visits == limit {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        assert_eq!(result, Ok(()));
        assert_eq!(visits, limit);
        let first: Vec<_> = in_range[..limit]
            .iter()
            .map(|(key, _)| key.clone())
            .collect();
        assert_eq!(keys, first);
    }

    // Breaking on the last record visits the whole range, and nothing more.
    let mut visits = 0;
    let result = tree.scan_with(None, None, |_, _| {
        visits += 1;
        match visits == all.len() {
            true => ControlFlow::Break(()),
            false => ControlFlow::Continue(()),
        }
    });
    assert_eq!((result, visits), (Ok(()), all.len()));
}

#[test]
fn scans_over_emptied_leaves_yield_exactly_the_live_records() {
    let (tree, all) = grown(600);
    // Merge every buffered insert first, so that the removes below only
    // buffer tombstones and no later merge splits a leaf.
    tree.flush().unwrap();
    let fences = boundaries(&tree);
    assert!(fences.len() > 10);
    // Empty whole leaves: the first, two adjacent ones, one in the middle,
    // and the last.
    let ranges: [Bounds<'_>; 4] = [
        (None, Some(fences[0].as_slice())),
        (Some(fences[3].as_slice()), Some(fences[5].as_slice())),
        (Some(fences[8].as_slice()), Some(fences[9].as_slice())),
        (fences.last().map(Vec::as_slice), None),
    ];
    let removed = |key: &[u8]| {
        ranges.iter().any(|&(start, end)| {
            start.is_none_or(|start| key >= start) && end.is_none_or(|end| key < end)
        })
    };
    let (dead, live): (Vec<_>, Vec<_>) = all.iter().cloned().partition(|(key, _)| removed(key));
    assert!(!dead.is_empty());
    for (key, _) in &dead {
        assert_eq!(tree.remove(key), Ok(()));
    }
    assert_eq!(tree.check_invariants(), Ok(()));
    let leaves = tree.leaf_count();

    // Tombstones hide the records while buffered, and the merges drop them.
    for flushed in [false, true] {
        if flushed {
            tree.flush().unwrap();
            assert_eq!(tree.check_invariants(), Ok(()));
            assert_eq!(tree.leaf_count(), leaves, "removes never split");
            assert_eq!(boundaries(&tree), fences);
            assert_eq!(empty_leaves(&tree), 5);
        }
        assert_eq!(scanned(&tree, None, None), live, "flushed: {flushed}");
        assert_eq!(visited(&tree, None, None), live);
        assert_eq!(collect(tree.iter()), live);
        // Bounds inside, at, and around every emptied leaf.
        let bounds: Vec<Option<&[u8]>> =
            fences.iter().map(|fence| Some(fence.as_slice())).collect();
        for start in bounds.iter().copied().chain([None]) {
            for end in bounds.iter().copied().chain([None]) {
                if start.zip(end).is_some_and(|(start, end)| start > end) {
                    continue;
                }
                let records = expected(&live, start, end);
                assert_eq!(scanned(&tree, start, end), records);
                assert_eq!(visited(&tree, start, end), records);
            }
        }
        // Ranges entirely inside emptied leaves are empty.
        for (start, end) in [
            (None, Some(fences[0].as_slice())),
            (Some(fences[3].as_slice()), Some(fences[5].as_slice())),
        ] {
            assert_eq!(scanned(&tree, start, end), []);
            assert_eq!(visited(&tree, start, end), []);
        }
    }
}

#[test]
fn get_with_calls_f_once_and_returns_its_result() {
    let tree = two_level();
    let mut calls = 0;
    let result = tree.get_with(b"h", |value| {
        calls += 1;
        value.map(<[u8]>::len)
    });
    assert_eq!((result, calls), (Ok(Some(value_of(b"h").len())), 1));

    let mut calls = 0;
    let result = tree.get_with(b"g", |value| {
        calls += 1;
        value.is_none()
    });
    assert_eq!((result, calls), (Ok(true), 1));

    // A rejected key never calls `f`.
    let mut calls = 0;
    let result = tree.get_with(b"", |_| calls += 1);
    assert_eq!((result, calls), (Err(TreeError::EmptyKey), 0));
}
