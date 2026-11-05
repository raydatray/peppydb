//! Reads, scans, and flush through hand-built mini-pages, before blind writes
//! create them.

use bf_tree_page::RecordState;

use super::*;

fn small() -> Tree {
    Tree::in_memory(config(256, 128, 8, 16)).unwrap()
}

fn leaf0() -> LeafId {
    LeafId::new(0).unwrap()
}

#[test]
fn reads_hit_the_mini_page_and_fall_back_to_the_base() {
    let tree = small();
    for key in [&b"a"[..], b"b", b"c"] {
        tree.put(key, b"base").unwrap();
    }
    tree.leaves
        .install_mini(
            leaf0(),
            &[
                (b"b", RecordState::Insert(b"mini")),
                (b"c", RecordState::Delete),
                (b"d", RecordState::Insert(b"new")),
            ],
        )
        .unwrap();
    assert_eq!(tree.buffered_leaves(), 1);

    assert_eq!(tree.get(b"a"), Ok(Some(b"base".to_vec())), "miss: base");
    assert_eq!(tree.get(b"b"), Ok(Some(b"mini".to_vec())), "mini wins");
    assert_eq!(tree.get(b"c"), Ok(None), "tombstone hides the base");
    assert_eq!(tree.get(b"d"), Ok(Some(b"new".to_vec())));
    assert_eq!(tree.get(b"e"), Ok(None));
    assert_eq!(tree.check_invariants(), Ok(()));

    let all: Vec<_> = tree.iter().collect::<Result<_, _>>().unwrap();
    let expected = [(&b"a"[..], &b"base"[..]), (b"b", b"mini"), (b"d", b"new")]
        .map(|(k, v)| (k.to_vec(), v.to_vec()));
    assert_eq!(all, expected);

    let mut visited = Vec::new();
    tree.scan_with(Some(b"b"), Some(b"d"), |key, value| {
        visited.push((key.to_vec(), value.to_vec()));
        std::ops::ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(visited, [(b"b".to_vec(), b"mini".to_vec())]);
}

#[test]
fn flush_merges_in_place_and_keeps_every_write_after_reopen() {
    let tree = small();
    tree.put(b"a", b"base").unwrap();
    tree.put(b"c", b"base").unwrap();
    tree.leaves
        .install_mini(
            leaf0(),
            &[
                (b"b", RecordState::Insert(b"new")),
                (b"c", RecordState::Delete),
            ],
        )
        .unwrap();
    let used = tree.circular_buffer_used();
    assert!(used > 0);

    tree.flush().unwrap();
    assert_eq!(tree.buffered_leaves(), 0);
    assert_eq!(tree.circular_buffer_used(), 0);
    assert_eq!(tree.leaf_count(), 1, "the merge fit in place");

    let reopened = Tree::open(tree.into_store()).unwrap();
    let all: Vec<_> = reopened.iter().collect::<Result<_, _>>().unwrap();
    let expected =
        [(&b"a"[..], &b"base"[..]), (b"b", b"new")].map(|(k, v)| (k.to_vec(), v.to_vec()));
    assert_eq!(all, expected);
    assert_eq!(reopened.check_invariants(), Ok(()));
}

#[test]
fn flush_splits_a_leaf_whose_merge_does_not_fit() {
    let tree = small();
    let key = |i: u8| [b'k', i];
    // 256 - 32 header - 16 fence slots = 208 bytes; 8 records of
    // 8 + 2 + 12 = 22 bytes fill 176 of them.
    for i in 0..8 {
        tree.put(&key(i * 2), &[1; 12]).unwrap();
    }
    assert_eq!(tree.leaf_count(), 1);
    // Four more (88 bytes) do not fit the base: the merge must split.
    let values: Vec<[u8; 12]> = (0..4).map(|i| [2 + i; 12]).collect();
    let records: Vec<(Vec<u8>, &[u8])> = (0..4)
        .map(|i| (key(i * 2 + 1).to_vec(), values[i as usize].as_slice()))
        .collect();
    let refs: Vec<_> = records
        .iter()
        .map(|(k, v)| (k.as_slice(), RecordState::Insert(v)))
        .collect();
    tree.leaves.install_mini(leaf0(), &refs).unwrap();

    tree.flush().unwrap();
    assert_eq!(tree.leaf_count(), 2, "the merge split");
    assert_eq!(tree.height(), 2);
    assert_eq!(tree.buffered_leaves(), 0);
    assert_eq!(tree.check_invariants(), Ok(()));
    for i in 0..8 {
        assert_eq!(tree.get(&key(i * 2)), Ok(Some(vec![1; 12])));
    }
    for (k, v) in &records {
        assert_eq!(tree.get(k), Ok(Some(v.to_vec())));
    }

    let reopened = Tree::open(tree.into_store()).unwrap();
    assert_eq!(reopened.iter().count(), 12);
    assert_eq!(reopened.check_invariants(), Ok(()));
}

#[test]
fn into_store_without_flush_loses_buffered_records() {
    let tree = small();
    tree.put(b"a", b"base").unwrap();
    tree.leaves
        .install_mini(leaf0(), &[(b"b", RecordState::Insert(b"buffered"))])
        .unwrap();
    let reopened = Tree::open(tree.into_store()).unwrap();
    assert_eq!(reopened.get(b"a"), Ok(Some(b"base".to_vec())));
    assert_eq!(reopened.get(b"b"), Ok(None));
}

#[test]
fn a_mini_page_naming_another_leaf_fails_the_invariants() {
    let tree = small();
    tree.leaves
        .install_mini(leaf0(), &[(b"a", RecordState::Insert(b"x"))])
        .unwrap();
    assert_eq!(tree.check_invariants(), Ok(()));
    // Rewrite the backing leaf in the mini-page's header.
    tree.leaves
        .corrupt_mini_backing(leaf0(), LeafId::new(9).unwrap());
    assert_eq!(
        tree.check_invariants(),
        Err(InvariantError::BadMiniPage(leaf0()))
    );
}
