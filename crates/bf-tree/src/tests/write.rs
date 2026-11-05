use std::collections::BTreeMap;

use super::*;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

/// Check the invariants, every model entry, and the total record count.
fn assert_model(tree: &Tree, model: &Model) {
    assert_eq!(tree.check_invariants(), Ok(()));
    for (key, value) in model {
        assert_eq!(tree.get(key), Ok(Some(value.clone())), "{key:?}");
    }
    let records: usize = (0..tree.leaf_count())
        .map(|index| {
            let bytes = tree
                .leaves
                .page_copy(LeafId::new(index as u64).unwrap())
                .unwrap();
            Page::validate(&bytes).unwrap().len()
        })
        .sum();
    assert_eq!(records, model.len());
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
        assert!(tree.height() >= 3, "height {}", tree.height());
        assert_model(&tree, &model);
        assert_eq!(tree.get(&key(600)), Ok(None));
    }
}

#[test]
fn replacement_can_force_a_split() {
    let mut tree = small();
    let mut model = Model::new();
    // 256 - 32 - 16 = 208 bytes: 7 * (8 + 4 + 16) + (8 + 4) fills the leaf.
    for i in 0..7 {
        put(&mut tree, &mut model, &key(i), &[1; 16]);
    }
    put(&mut tree, &mut model, &key(7), &[]);
    let bytes = tree.leaves.page_copy(LeafId::new(0).unwrap()).unwrap();
    assert_eq!(Page::validate(&bytes).unwrap().free_bytes(), 0);
    assert_eq!(tree.height(), 1);
    put(&mut tree, &mut model, &key(7), &[2; 16]);
    assert_eq!(tree.height(), 2);
    assert_model(&tree, &model);
}

#[test]
fn holes_are_compacted_before_splitting() {
    let mut tree = small();
    let mut model = Model::new();
    for i in 0..10 {
        put(&mut tree, &mut model, &key(i), &[1; 8]);
    }
    // Shrinking replacements leave holes.
    for i in 0..10 {
        put(&mut tree, &mut model, &key(i), &[]);
    }
    // 10 * 12 live bytes plus 4 * 20 new ones still fit in 208.
    for i in 10..14 {
        put(&mut tree, &mut model, &key(i), &[3; 8]);
    }
    assert_eq!(tree.height(), 1);
    assert_eq!(tree.leaf_count(), 1);
    assert_model(&tree, &model);
}

#[test]
fn removal_empties_leaves_without_merging() {
    let mut tree = small();
    let mut model = Model::new();
    for i in 0..300 {
        put(&mut tree, &mut model, &key(i), &[7; 4]);
    }
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
    let mut tree = small();
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
    // Every record is as large as the admission rule allows.
    let configs = [
        config(128, 64, 2, 14),
        config(128, 64, 8, 0),
        config(256, 64, 12, 33),
        config(4096, 4096, 64, 1192),
        config(4096, 128, 44, 1238),
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
    for i in 0..7 {
        put(&mut tree, &mut model, &key(i), &[1; 16]);
    }
    put(&mut tree, &mut model, &key(7), &[]);
    let bytes = tree.leaves.page_copy(LeafId::new(0).unwrap()).unwrap();
    assert_eq!(Page::validate(&bytes).unwrap().free_bytes(), 0);

    for i in [3, 9, 0] {
        tree.remove(&key(i)).unwrap();
        model.remove(key(i).as_slice());
        assert_eq!((tree.leaf_count(), tree.height()), (1, 1));
    }
    assert_model(&tree, &model);
}
