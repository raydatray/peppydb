use std::collections::BTreeMap;

use super::*;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

/// Check the invariants and every model entry, through the mini-pages and
/// then again after a flush; then check that the base pages hold exactly the
/// model's records.
fn assert_model(tree: &Tree, model: &Model) {
    for flushed in [false, true] {
        if flushed {
            tree.checkpoint().unwrap();
            assert_eq!(tree.buffered_leaves(), 0);
            assert_eq!(tree.circular_buffer_used(), 0);
        }
        assert_eq!(tree.check_invariants(), Ok(()));
        for (key, value) in model {
            assert_eq!(tree.get(key), Ok(Some(value.clone())), "{key:?}");
        }
        assert_eq!(tree.iter().count(), model.len());
    }
    assert_eq!(base_records(tree), model.len());
}

/// Records in every base page, buffered ones excluded.
fn base_records(tree: &Tree) -> usize {
    (0..tree.leaf_count())
        .map(|index| {
            let bytes = tree
                .leaves
                .page_copy(LeafId::new(index as u64).unwrap())
                .unwrap();
            Page::validate(&bytes).unwrap().len()
        })
        .sum()
}

/// Leaf 0's base page, the only leaf of a one-leaf tree.
fn root_free_bytes(tree: &Tree) -> usize {
    let bytes = tree.leaves.page_copy(LeafId::new(0).unwrap()).unwrap();
    Page::validate(&bytes).unwrap().free_bytes()
}

fn put(tree: &mut Tree, model: &mut Model, key: &[u8], value: &[u8]) {
    model.insert(key.to_vec(), value.to_vec());
    assert_eq!(tree.put(key, value), Ok(()), "{key:?}");
    assert_eq!(tree.get(key), Ok(Some(value.to_vec())), "{key:?}");
}

/// Small nodes reach several levels after a few hundred records.
fn small() -> Tree {
    Tree::in_memory(config(256, 128, 8, 16)).unwrap()
}

fn key(index: u32) -> [u8; 4] {
    index.to_be_bytes()
}

#[test]
fn ordered_inserts_grow_several_levels() {
    let orders: [Box<dyn Fn(u32) -> u32>; 3] = [
        Box::new(|i| i),
        Box::new(|i| 599 - i),
        // A stride coprime with 600 interleaves the whole key space.
        Box::new(|i| (i * 257) % 600),
    ];
    for order in orders {
        let mut tree = small();
        let mut model = Model::new();
        for i in 0..600 {
            put(&mut tree, &mut model, &key(order(i)), &[i as u8; 9]);
            assert_eq!(tree.check_invariants(), Ok(()));
        }
        assert_model(&tree, &model);
        assert!(tree.height() >= 3, "height {}", tree.height());
        assert_eq!(tree.get(&key(600)), Ok(None));
    }
}

/// A one-leaf tree whose flushed base page is full: 256 - 32 - 16 = 208
/// bytes, and 7 * (8 + 4 + 16) + (8 + 4) fill them.
fn full_leaf(tree: &mut Tree, model: &mut Model) {
    for i in 0..7 {
        put(tree, model, &key(i), &[1; 16]);
    }
    put(tree, model, &key(7), &[]);
    tree.checkpoint().unwrap();
    assert_eq!(root_free_bytes(tree), 0);
    assert_eq!((tree.leaf_count(), tree.height()), (1, 1));
}

#[test]
fn replacement_can_force_a_split() {
    let mut tree = small();
    let mut model = Model::new();
    full_leaf(&mut tree, &mut model);
    // The growing replacement is buffered; the merge that applies it does
    // not fit the full base page and splits it.
    put(&mut tree, &mut model, &key(7), &[2; 16]);
    assert_eq!(tree.height(), 1);
    tree.checkpoint().unwrap();
    assert_eq!(tree.height(), 2);
    assert_model(&tree, &model);
}

#[test]
fn holes_are_compacted_before_growing_or_merging() {
    let mut tree = small();
    let mut model = Model::new();
    let leaf0 = LeafId::new(0).unwrap();
    let mini = |tree: &Tree| tree.leaves.mini_copy(leaf0).expect("a mini-page");
    // The ladder is [64, 128]: four 20-byte records use 80 of the 96 record
    // bytes of the largest class.
    for i in 0..4 {
        put(&mut tree, &mut model, &key(i), &[1; 8]);
    }
    // Shrinking replacements leave holes in the mini-page.
    for i in 0..4 {
        put(&mut tree, &mut model, &key(i), &[]);
    }
    let bytes = mini(&tree);
    let page = Page::validate(&bytes).unwrap();
    assert_eq!(page.capacity(), 128);
    assert_eq!(page.reclaimable_bytes(), 32);
    // 4 * 12 live bytes plus 2 * 20 new ones fit 96 only after compaction.
    for i in 4..6 {
        put(&mut tree, &mut model, &key(i), &[3; 8]);
    }
    let bytes = mini(&tree);
    let page = Page::validate(&bytes).unwrap();
    assert_eq!((page.capacity(), page.len()), (128, 6));
    assert_eq!(page.reclaimable_bytes(), 0);
    // Nothing merged: the base page is still empty.
    assert_eq!(base_records(&tree), 0);
    assert_eq!(tree.leaf_count(), 1);
    assert_model(&tree, &model);
    assert_eq!((tree.leaf_count(), tree.height()), (1, 1));
}

#[test]
fn removal_empties_leaves_without_merging() {
    let mut tree = small();
    let mut model = Model::new();
    for i in 0..300 {
        put(&mut tree, &mut model, &key(i), &[7; 4]);
    }
    // Merge the inserts, so that the removes below only buffer tombstones.
    assert_model(&tree, &model);
    let leaves = tree.leaf_count();
    let height = tree.height();
    for i in (0..300).step_by(2).chain((1..300).step_by(2)) {
        assert_eq!(tree.get(&key(i)), Ok(Some(vec![7; 4])));
        assert_eq!(tree.remove(&key(i)), Ok(()));
        assert_eq!(tree.get(&key(i)), Ok(None));
        assert_eq!(tree.remove(&key(i)), Ok(()));
        model.remove(key(i).as_slice());
        assert_eq!(tree.check_invariants(), Ok(()));
    }
    // The merges, during the removes and at the flush, drop the tombstones
    // and empty every base page without merging leaves.
    assert_model(&tree, &model);
    assert_eq!((tree.leaf_count(), tree.height()), (leaves, height));
    for i in 0..300 {
        assert_eq!(tree.get(&key(i)), Ok(None));
    }
    // Empty leaves keep their fences and accept records again.
    for i in (0..300).rev() {
        put(&mut tree, &mut model, &key(i), &[8; 4]);
    }
    assert_model(&tree, &model);
}

#[test]
fn limits_are_checked_before_touching_nodes() {
    let tree = small();
    tree.put(b"a", b"1").unwrap();
    let snapshot = tree.leaves.page_copy(LeafId::new(0).unwrap()).unwrap();
    assert_eq!(tree.put(b"", b"1"), Err(TreeError::EmptyKey));
    assert_eq!(tree.put(&[1; 9], b"1"), Err(TreeError::KeyTooLarge));
    assert_eq!(tree.put(b"b", &[1; 17]), Err(TreeError::ValueTooLarge));
    assert_eq!(tree.remove(b""), Err(TreeError::EmptyKey));
    assert_eq!(tree.remove(&[1; 9]), Err(TreeError::KeyTooLarge));
    assert_eq!(
        tree.leaves.page_copy(LeafId::new(0).unwrap()).unwrap(),
        snapshot
    );
    assert_eq!(tree.get(b"b"), Ok(None));
    assert_eq!(tree.put(&[1; 8], &[1; 16]), Ok(()));
    assert_eq!(tree.get(&[1; 8]), Ok(Some(vec![1; 16])));
    assert_eq!(tree.put(b"b", b""), Ok(()));
    assert_eq!(tree.get(b"b"), Ok(Some(Vec::new())));
}

#[test]
fn maximum_records_split_at_every_level() {
    // Every record is as large as the ladder rule allows: one more value
    // byte fails `32 + 8 + K + V <= floor64(L - 5K - V - 24)`, or for
    // (128, 8, 0), `M >= 64`.
    let configs = [
        config(128, 64, 2, 22),
        config(128, 64, 8, 0),
        config(256, 64, 12, 44),
        config(4096, 4096, 64, 1816),
        config(4096, 128, 44, 1868),
    ];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    for config in configs {
        assert_eq!(config.validate(), Ok(()), "{config:?}");
        let bigger = TreeConfig {
            max_value_len: config.max_value_len + 1,
            ..config
        };
        assert!(bigger.validate().is_err(), "{bigger:?}");
        let mut tree = Tree::in_memory(config).unwrap();
        let mut model = Model::new();
        for _ in 0..300 {
            let key: Vec<u8> = (0..config.max_key_len).map(|_| next(3) as u8).collect();
            let value = vec![next(256) as u8; config.max_value_len];
            put(&mut tree, &mut model, &key, &value);
        }
        assert_model(&tree, &model);
        assert!(tree.height() >= 2, "{config:?}");
    }
}

#[test]
fn removing_from_a_full_leaf_never_splits() {
    let mut tree = small();
    let mut model = Model::new();
    full_leaf(&mut tree, &mut model);

    // Removes buffer tombstones, even for the absent key 9.
    for i in [3, 9, 0] {
        tree.remove(&key(i)).unwrap();
        model.remove(key(i).as_slice());
        assert_eq!(tree.get(&key(i)), Ok(None));
        assert_eq!((tree.leaf_count(), tree.height()), (1, 1));
    }
    assert_eq!(base_records(&tree), 8, "nothing merged yet");
    // The merge drops the tombstones and the records they hide, in place.
    assert_model(&tree, &model);
    assert_eq!((tree.leaf_count(), tree.height()), (1, 1));
    assert_eq!(root_free_bytes(&tree), 2 * (8 + 4 + 16));
}

#[test]
fn threads_share_a_tree_through_a_shared_reference() {
    const THREADS: u32 = 4;
    const PER_THREAD: u32 = 150;
    let tree = small();
    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let tree = &tree;
            scope.spawn(move || {
                // Interleave the threads' keys so that they share leaves.
                for i in 0..PER_THREAD {
                    let k = key(i * THREADS + thread);
                    tree.put(&k, &k).unwrap();
                }
            });
        }
    });

    assert_eq!(tree.check_invariants(), Ok(()));
    assert!(tree.height() >= 2);
    for i in 0..THREADS * PER_THREAD {
        assert_eq!(tree.get(&key(i)), Ok(Some(key(i).to_vec())), "{i}");
    }
    assert_eq!(tree.iter().count(), (THREADS * PER_THREAD) as usize);
}
