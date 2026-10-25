use zerocopy::FromBytes;

use super::*;
use crate::tree::store::Store;

#[test]
fn defaults_validate() {
    let defaults = TreeConfig::default();
    assert_eq!(defaults, config(4096, 4096, 64, 1024));
    assert_eq!(defaults.validate(), Ok(()));
}

#[test]
fn capacities_follow_the_page_rule() {
    for capacity in [0, 63, 65, 96, 4160, 8192] {
        assert!(matches!(
            config(capacity, 4096, 1, 0).validate(),
            Err(TreeError::InvalidConfig(_))
        ));
        assert!(matches!(
            config(4096, capacity, 1, 0).validate(),
            Err(TreeError::InvalidConfig(_))
        ));
    }
    assert!(matches!(
        config(4096, 4096, 0, 0).validate(),
        Err(TreeError::InvalidConfig(_))
    ));
}

#[test]
fn leaf_rule_is_tight_on_both_sides() {
    // 7K + 3V <= L - 72.
    let cases = [
        (4096, 64, 1192, true),
        (4096, 64, 1193, false),
        (4096, 574, 0, true),
        (4096, 575, 0, false),
        (128, 8, 0, true),
        (128, 8, 1, false),
        (128, 2, 14, true),
        (128, 2, 15, false),
        // 64-byte leaves cannot satisfy the rule at all.
        (64, 1, 0, false),
        (4096, usize::MAX, usize::MAX, false),
    ];
    for (leaf, key, value, valid) in cases {
        let result = config(leaf, 4096, key, value).validate();
        assert_eq!(result.is_ok(), valid, "L={leaf} K={key} V={value}");
    }
}

#[test]
fn inner_rule_is_tight_on_both_sides() {
    // 16 + max(K - 4, 0) <= (I - 16) / 2.
    let cases = [
        (64, 12, true),
        (64, 13, false),
        (128, 44, true),
        (128, 45, false),
        (4096, 574, true),
    ];
    for (inner, key, valid) in cases {
        let result = config(4096, inner, key, 0).validate();
        assert_eq!(result.is_ok(), valid, "I={inner} K={key}");
    }
}

#[test]
fn new_tree_is_one_unbounded_leaf() {
    let tree = Tree::new(config(256, 128, 8, 16)).unwrap();
    let ChildRef::Leaf(root) = tree.root else {
        panic!("root must be a leaf");
    };
    let page = Page::ref_from_bytes(tree.leaves.get(root)).unwrap();
    assert_eq!(page.capacity(), 256);
    assert!(page.is_empty());
    assert_eq!(
        page.fences(),
        Some(Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded
        })
    );
    assert_eq!(tree.leaves.len(), 1);
    assert_eq!(tree.inner.len(), 0);
    assert_eq!(tree.config(), &config(256, 128, 8, 16));

    assert!(matches!(
        Tree::new(config(64, 64, 1, 0)),
        Err(TreeError::InvalidConfig(_))
    ));
}

#[test]
fn store_allocates_distinct_zeroed_disjoint_buffers() {
    let mut store: Store<LeafId> = Store::new(64);
    let ids: Vec<_> = (0..4).map(|_| store.alloc()).collect();
    let indices: Vec<_> = ids.iter().map(|id| id.index()).collect();
    assert_eq!(indices, [0, 1, 2, 3]);
    assert_eq!(store.len(), 4);
    assert!(ids.iter().all(|&id| store.get(id) == [0; 64]));

    store.get_mut(ids[1]).fill(1);
    let (a, b) = store.pair_mut(ids[3], ids[1]);
    assert_eq!(b, [1; 64]);
    a.fill(3);
    b.fill(2);
    assert_eq!(store.get(ids[0]), [0; 64]);
    assert_eq!(store.get(ids[1]), [2; 64]);
    assert_eq!(store.get(ids[3]), [3; 64]);
    assert_eq!(LeafId::from_index(5).raw(), 5);
}

#[test]
#[should_panic(expected = "distinct allocated IDs")]
fn store_pair_rejects_aliasing() {
    let mut store: Store<InnerId> = Store::new(64);
    let id = store.alloc();
    store.pair_mut(id, id);
}
