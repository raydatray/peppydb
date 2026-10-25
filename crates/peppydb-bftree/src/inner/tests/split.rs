use super::*;
use crate::inner::split::choose_promotion;

fn entry_bytes(key: &[u8]) -> usize {
    8 + key.len().saturating_sub(4) + 8
}

fn separators(node: &InnerNode) -> Vec<(Vec<u8>, ChildRef)> {
    node.iter()
        .map(|entry| (entry.key.to_vec(), entry.right_child))
        .collect()
}

/// Build a leaf-child node whose separator `i` has right child `leaf(raw_i)`.
fn build<'a>(storage: &'a mut [u8], entries: &[(&[u8], u64)]) -> &'a mut InnerNode {
    let node = InnerNode::init(storage, leaf(0)).unwrap();
    for &(key, raw) in entries {
        node.try_insert(key, leaf(raw)).unwrap();
    }
    node
}

/// Check that a parent holding only the promoted separator, over the two
/// outputs, routes every query to the child the unsplit sequence chooses.
fn assert_composed_routing(
    first: ChildRef,
    expected: &[(Vec<u8>, ChildRef)],
    left: &InnerNode,
    split: &InnerSplit<'_>,
) {
    let mut queries: Vec<Vec<u8>> = vec![b"".to_vec(), vec![0xff; 80]];
    for (key, _) in expected {
        queries.push(key.clone());
        queries.push([key.as_slice(), b"\0"].concat());
        queries.push(key[..key.len() - 1].to_vec());
    }
    for query in queries {
        let unsplit = expected
            .iter()
            .rev()
            .find(|(key, _)| key.as_slice() <= query.as_slice())
            .map_or(first, |&(_, child)| child);
        let side = if query < split.separator {
            left
        } else {
            &*split.right
        };
        assert_eq!(side.route(&query).child, unsplit, "{query:?}");
    }
}

#[test]
fn pending_entry_before_inside_or_after_existing_entries() {
    let existing: [(&[u8], u64); 4] = [(b"b", 1), (b"d", 2), (b"f", 3), (b"h", 4)];
    for pending in [&b"a"[..], b"c", b"e", b"g", b"i"] {
        let mut storage = [0; 128];
        let mut right_storage = [0xa5; 128];
        let node = build(&mut storage, &existing);

        let mut expected = separators(node);
        let position = expected.partition_point(|(key, _)| key.as_slice() < pending);
        expected.insert(position, (pending.to_vec(), leaf(99)));

        let split = node
            .split_with_insert(&mut right_storage, pending, leaf(99))
            .unwrap();
        // Equal sizes: the count midpoint wins.
        assert_eq!(split.separator, expected[2].0);
        assert_eq!(split.right.first_child(), expected[2].1);
        assert_eq!(separators(split.right), expected[3..]);
        assert_composed_routing(leaf(0), &expected, node, &split);

        assert_eq!(node.first_child(), leaf(0));
        assert_eq!(separators(node), expected[..2]);
        assert_eq!(node.free_bytes(), 128 - 16 - 32);
        assert_eq!(split.right.free_bytes(), 128 - 16 - 32);
        assert_eq!(split.right.capacity(), 128);
    }
}

#[test]
fn promote_the_pending_key() {
    let mut storage = [0; 64];
    let mut right_storage = [0; 64];
    let node = build(&mut storage, &[(b"aaaa", 1), (b"cccc", 2)]);
    let split = node
        .split_with_insert(&mut right_storage, b"bbbb", leaf(9))
        .unwrap();

    assert_eq!(split.separator, b"bbbb");
    assert_eq!(split.right.first_child(), leaf(9));
    assert_eq!(separators(split.right), [(b"cccc".to_vec(), leaf(2))]);
    assert_eq!(separators(node), [(b"aaaa".to_vec(), leaf(1))]);
    assert_eq!(node.child_count(), 2);
    assert_eq!(split.right.child_count(), 2);
}

#[test]
fn byte_skewed_keys_choose_the_smaller_maximum() {
    // The plan's example: entry sizes [64, 16, 16, 16] fill a 128-byte node.
    let long = [b'a'; 52];
    let mut storage = [0; 128];
    let mut right_storage = [0; 128];
    let node = build(&mut storage, &[(&long, 1), (b"b", 2), (b"c", 3), (b"d", 4)]);
    assert_eq!(node.free_bytes(), 0);
    assert_eq!(node.try_insert(b"e", leaf(5)), Err(InnerError::NodeFull));

    let split = node
        .split_with_insert(&mut right_storage, b"e", leaf(5))
        .unwrap();
    // Promoting "b" leaves 80 and 64 used bytes; the midpoint "c" leaves 96/48.
    assert_eq!(split.separator, b"b");
    assert_eq!(node.free_bytes(), 128 - 80);
    assert_eq!(split.right.free_bytes(), 128 - 64);
    assert_eq!(split.right.first_child(), leaf(2));
}

#[test]
fn candidate_scoring_excludes_endpoints_and_oversized_outputs() {
    let choose = |sizes: &[usize], capacity| choose_promotion(sizes.iter().copied(), capacity);
    assert_eq!(choose(&[], 128), None);
    assert_eq!(choose(&[16, 16], 128), None);
    assert_eq!(choose(&[16, 16, 16], 128), Some(1));
    assert_eq!(choose(&[64, 16, 16, 16, 16], 128), Some(1));
    // Ties on the maximum break toward the smaller difference, then the index.
    assert_eq!(choose(&[16, 16, 16, 16], 128), Some(1));
    assert_eq!(choose(&[16, 32, 16, 16, 32], 256), Some(2));
    // Every candidate overflows a side.
    assert_eq!(choose(&[40, 40, 40, 40], 64), None);
}

#[test]
fn too_few_separators_cannot_split() {
    // One 36-byte key fills a 64-byte node; two separators cannot split.
    let mut storage = [0; 64];
    let mut right_storage = [0xa5; 64];
    let node = build(&mut storage, &[(&[b'k'; 36], 1)]);
    let snapshot = node.as_bytes().to_vec();
    assert_eq!(
        node.split_with_insert(&mut right_storage, b"a", leaf(2))
            .err(),
        Some(InnerError::CannotSplit)
    );
    assert_eq!(node.as_bytes(), snapshot);
    assert_eq!(right_storage, [0xa5; 64]);

    let node = InnerNode::init(&mut storage, leaf(0)).unwrap();
    assert_eq!(
        node.split_with_insert(&mut right_storage, b"a", leaf(2))
            .err(),
        Some(InnerError::CannotSplit)
    );
}

#[test]
fn errors_follow_precedence_and_preserve_both_buffers() {
    let mut storage = [0; 128];
    let node = build(&mut storage, &[(b"b", 1), (b"d", 2), (b"f", 3)]);
    let snapshot = node.as_bytes().to_vec();

    let too_large = [b'z'; 101];
    let cases: [(usize, &[u8], ChildRef, InnerError); 8] = [
        (96, b"", inner(1), InnerError::InvalidCapacity),
        (4160, b"c", leaf(1), InnerError::InvalidCapacity),
        (64, b"", inner(1), InnerError::CapacityMismatch),
        (256, b"c", leaf(1), InnerError::CapacityMismatch),
        (128, b"", inner(1), InnerError::EmptyKey),
        (128, &too_large, inner(1), InnerError::ChildKindMismatch),
        (128, &too_large, leaf(1), InnerError::SeparatorTooLarge),
        (128, b"d", leaf(1), InnerError::DuplicateSeparator),
    ];
    for (capacity, key, child, error) in cases {
        let mut right_storage = vec![0xa5; capacity];
        assert_eq!(
            node.split_with_insert(&mut right_storage, key, child).err(),
            Some(error)
        );
        assert_eq!(node.as_bytes(), snapshot);
        assert_eq!(right_storage, vec![0xa5; capacity]);
    }
}

#[test]
fn split_before_the_node_is_full() {
    let mut storage = [0; 4096];
    let mut right_storage = [0; 4096];
    let node = InnerNode::init(&mut storage, inner(0)).unwrap();
    for raw in 1..=10u64 {
        node.try_insert(&[b'k', raw as u8], inner(raw)).unwrap();
    }
    let split = node
        .split_with_insert(&mut right_storage, b"k\x0b", inner(11))
        .unwrap();
    assert_eq!(split.separator, b"k\x06");
    assert_eq!(node.len() + split.right.len() + 1, 11);
    assert_eq!(split.right.first_child(), inner(6));
}

#[test]
fn half_budget_admission_always_splits() {
    // Every entry fits within half the payload budget, so a full node with at
    // least two separators always splits with one pending entry.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |bound: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % bound as u64) as usize
    };
    for capacity in [64, 128, 256, 512] {
        let max_key = (capacity - 16) / 2 - 16 + 4;
        for _ in 0..200 {
            let mut storage = vec![0; capacity];
            let mut right_storage = vec![0; capacity];
            let node = InnerNode::init(&mut storage, leaf(0)).unwrap();
            let mut entries = Vec::new();
            let pending = loop {
                let key: Vec<u8> = (0..1 + next(max_key)).map(|_| next(4) as u8).collect();
                assert!(entry_bytes(&key) <= (capacity - 16) / 2);
                match node.try_insert(&key, leaf(entries.len() as u64 + 1)) {
                    Ok(()) => entries.push(key),
                    Err(InnerError::DuplicateSeparator) => {}
                    Err(InnerError::NodeFull) => break key,
                    Err(error) => panic!("{error}"),
                }
            };
            assert!(node.len() >= 2);
            let split = node
                .split_with_insert(&mut right_storage, &pending, leaf(1000))
                .unwrap();
            assert!(!node.is_empty() && !split.right.is_empty());
            assert_eq!(node.len() + split.right.len(), entries.len());
        }
    }
}
