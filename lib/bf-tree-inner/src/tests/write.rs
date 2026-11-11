use super::*;

/// Entry bytes from the documented rule: slot, tail beyond four inline bytes,
/// and an eight-byte child.
fn entry_bytes(key: &[u8]) -> usize {
    8 + key.len().saturating_sub(4) + 8
}

fn separators(node: &InnerNode) -> Vec<(Vec<u8>, ChildRef)> {
    node.iter()
        .map(|entry| (entry.key.to_vec(), entry.right_child))
        .collect()
}

#[test]
fn insert_at_every_position_preserves_neighbors() {
    let keys: [&[u8]; 4] = [b"b", b"dddd", b"ffffff", b"h"];
    let probes: [&[u8]; 5] = [b"a", b"c", b"e", b"g", b"i"];
    for (position, probe) in probes.into_iter().enumerate() {
        let mut storage = [0; 256];
        let node = InnerNode::init(&mut storage, leaf(0)).unwrap();
        for (key, raw) in keys.iter().zip(1..) {
            node.try_insert(key, leaf(raw)).unwrap();
        }
        let before = separators(node);

        node.try_insert(probe, leaf(99)).unwrap();

        let mut expected = before;
        expected.insert(position, (probe.to_vec(), leaf(99)));
        assert_eq!(separators(node), expected);
        assert_eq!(node.first_child(), leaf(0));
        // The child left of the new separator is unchanged.
        assert_eq!(node.route(probe).index, position + 1);
        let left = position.checked_sub(1).map_or(0, |index| index as u64 + 1);
        assert_eq!(node.child_at(position), Some(leaf(left)));
    }
}

#[test]
fn reverse_and_interleaved_insertion_orders_sort_separators() {
    let mut keys: Vec<Vec<u8>> = (0..40u8).map(|i| vec![b'k', i / 7, i % 7, i]).collect();
    keys.extend((0..10u8).map(|i| vec![b'k', i]));
    let mut storage = [0; 4096];
    let node = InnerNode::init(&mut storage, inner(0)).unwrap();
    for (key, raw) in keys.iter().rev().zip(1..) {
        node.try_insert(key, inner(raw)).unwrap();
    }

    let stored: Vec<_> = node.iter().map(|entry| entry.key.to_vec()).collect();
    keys.sort();
    assert_eq!(stored, keys);
    let used: usize = keys.iter().map(|key| entry_bytes(key)).sum();
    assert_eq!(node.free_bytes(), 4096 - 16 - used);
}

#[test]
fn short_and_long_keys_use_documented_space() {
    let mut storage = [0; 512];
    let node = InnerNode::init(&mut storage, leaf(0)).unwrap();
    let keys: [&[u8]; 7] = [
        b"\0",
        b"ab",
        b"abc",
        b"abcd",
        b"abcde",
        &[b'x'; 40],
        b"\0\0",
    ];
    let mut used = 0;
    for (key, raw) in keys.into_iter().zip(1..) {
        node.try_insert(key, leaf(raw)).unwrap();
        used += entry_bytes(key);
        assert_eq!(node.free_bytes(), 512 - 16 - used);
    }
    assert_eq!(node.len(), keys.len());
}

#[test]
fn exact_fits_and_full_nodes() {
    // A 64-byte node has 48 entry bytes: one 36-byte key fills it exactly.
    let mut storage = [0; 64];
    let node = InnerNode::init(&mut storage, leaf(0)).unwrap();
    assert_eq!(
        node.try_insert(&[b'k'; 37], leaf(1)),
        Err(InnerError::SeparatorTooLarge)
    );
    node.try_insert(&[b'k'; 36], leaf(1)).unwrap();
    assert_eq!(node.free_bytes(), 0);
    assert_eq!(node.try_insert(b"a", leaf(2)), Err(InnerError::NodeFull));

    // Three 16-byte entries also fill it.
    let node = InnerNode::init(&mut storage, leaf(0)).unwrap();
    for (key, raw) in [b"aaaa", b"bbbb", b"cccc"].into_iter().zip(1..) {
        node.try_insert(key, leaf(raw)).unwrap();
    }
    assert_eq!(node.free_bytes(), 0);
    assert_eq!(node.try_insert(b"d", leaf(4)), Err(InnerError::NodeFull));

    // Fixed-width replacement still succeeds on a full node.
    assert_eq!(node.replace_child(3, leaf(7)), Ok(leaf(3)));
    assert_eq!(node.replace_child(0, leaf(8)), Ok(leaf(0)));
    assert_eq!(node.free_bytes(), 0);
    assert_eq!(node.route(b"cccc").child, leaf(7));
}

#[test]
fn errors_follow_precedence_and_preserve_bytes() {
    let mut storage = [0; 64];
    let node = InnerNode::init(&mut storage, leaf(0)).unwrap();
    for (key, raw) in [b"aaaa", b"bbbb", b"cccc"].into_iter().zip(1..) {
        node.try_insert(key, leaf(raw)).unwrap();
    }
    let snapshot = node.as_bytes().to_vec();

    let too_large = [b'z'; 37];
    let cases: [(&[u8], ChildRef, InnerError); 6] = [
        (b"", inner(1), InnerError::EmptyKey),
        (b"", leaf(1), InnerError::EmptyKey),
        (&too_large, inner(1), InnerError::ChildKindMismatch),
        (&too_large, leaf(1), InnerError::SeparatorTooLarge),
        (b"bbbb", leaf(9), InnerError::DuplicateSeparator),
        (b"d", leaf(9), InnerError::NodeFull),
    ];
    for (key, child, error) in cases {
        assert_eq!(node.try_insert(key, child), Err(error), "{key:?}");
        assert_eq!(node.as_bytes(), snapshot);
    }
}

#[test]
fn replace_child_changes_only_the_selected_reference() {
    let mut storage = [0; 128];
    let node = InnerNode::init(&mut storage, inner(0)).unwrap();
    let keys: [&[u8]; 3] = [b"aaaaa", b"bb", b"ccccccc"];
    for (key, raw) in keys.into_iter().zip(1..) {
        node.try_insert(key, inner(raw)).unwrap();
    }

    for index in 0..4 {
        let before = node.as_bytes().to_vec();
        let replacement = inner(0x0101_0101_0101_0101 * (index as u64 + 1));
        assert_eq!(
            node.replace_child(index, replacement),
            Ok(inner(index as u64))
        );
        assert_eq!(node.child_at(index), Some(replacement));

        // Exactly the eight bytes of that child ID differ.
        let changed: Vec<_> = (0..128)
            .filter(|&at| before[at] != node.as_bytes()[at])
            .collect();
        assert_eq!(changed.len(), 8, "child {index}");
        assert_eq!(changed[7] - changed[0], 7);
        if index == 0 {
            assert_eq!(changed[0], 8);
        }

        node.replace_child(index, inner(index as u64)).unwrap();
        assert_eq!(node.as_bytes(), before);
    }
}

#[test]
fn replace_child_errors_preserve_bytes() {
    let mut storage = [0; 128];
    let node = InnerNode::init(&mut storage, leaf(0)).unwrap();
    node.try_insert(b"m", leaf(1)).unwrap();
    let snapshot = node.as_bytes().to_vec();

    let cases = [
        (2, leaf(5), InnerError::InvalidChildIndex),
        (2, inner(5), InnerError::InvalidChildIndex),
        (usize::MAX, leaf(5), InnerError::InvalidChildIndex),
        (0, inner(5), InnerError::ChildKindMismatch),
        (1, inner(5), InnerError::ChildKindMismatch),
    ];
    for (index, child, error) in cases {
        assert_eq!(node.replace_child(index, child), Err(error));
        assert_eq!(node.as_bytes(), snapshot);
    }
}
