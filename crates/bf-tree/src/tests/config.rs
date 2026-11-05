use super::*;
use crate::store::InnerStore;
use bf_tree_mini_page::SizeClasses;

#[test]
fn defaults_validate() {
    let defaults = TreeConfig::default();
    assert_eq!(
        defaults,
        TreeConfig {
            circular_buffer_bytes: 32 << 20,
            write_load_full_page: true,
            ..config(4096, 4096, 64, 1024)
        }
    );
    assert_eq!(defaults.validate(), Ok(()));
    assert_eq!(
        defaults.size_classes().unwrap().as_slice(),
        [64, 128, 256, 512, 1024, 2048, 2688]
    );
    assert_eq!(
        defaults.buffer_classes(&defaults.size_classes().unwrap()),
        [64, 128, 256, 512, 1024, 2048, 2688, 4096]
    );
}

#[test]
fn the_circular_buffer_budget_is_checked() {
    let base = config(256, 128, 8, 16);
    // At least 2 * (256 + 16) = 544, so 1024 is the smallest power of two.
    for bytes in [1024, 4096, 1 << 30] {
        let ok = TreeConfig {
            circular_buffer_bytes: bytes,
            ..base
        };
        assert_eq!(ok.validate(), Ok(()), "{bytes}");
    }
    for bytes in [0, 512, 1000, 3 << 20] {
        let bad = TreeConfig {
            circular_buffer_bytes: bytes,
            ..base
        };
        assert!(
            matches!(bad.validate(), Err(TreeError::InvalidConfig(_))),
            "{bytes}"
        );
    }
}

#[test]
fn limits_without_a_mini_page_ladder_are_rejected() {
    // M = floor64(192 - 5 - 37 - 24) = 64, but a record needs 32 + 8 + 1 + 37.
    let bad = config(192, 128, 1, 37);
    assert!(matches!(bad.validate(), Err(TreeError::InvalidConfig(_))));
}

#[test]
fn open_with_takes_runtime_settings_and_checks_the_layout() {
    let config = config(256, 128, 8, 16);
    let tree = Tree::in_memory(config).unwrap();
    tree.put(b"k", b"v").unwrap();
    // Buffered writes reach the store only through a merge.
    tree.flush().unwrap();
    let store = tree.into_store();

    // A different budget is a runtime setting.
    let bigger = TreeConfig {
        circular_buffer_bytes: 1 << 22,
        ..config
    };
    let tree = Tree::open_with(bigger, store).unwrap();
    assert_eq!(tree.config().circular_buffer_bytes, 1 << 22);
    assert_eq!(tree.get(b"k"), Ok(Some(b"v".to_vec())));

    // A different layout is rejected.
    let other = TreeConfig {
        max_value_len: 8,
        ..config
    };
    assert!(matches!(
        Tree::open_with(other, tree.into_store()),
        Err(TreeError::InvalidStore(_))
    ));
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

/// The old admission rule, `7K + 3V <= L - 72`, which direct base-page
/// writes needed before writes were buffered.
fn old_leaf_rule(leaf: usize, key: usize, value: usize) -> bool {
    leaf.checked_sub(72)
        .is_some_and(|room| 7 * key + 3 * value <= room)
}

#[test]
fn ladder_rule_is_tight_on_both_sides() {
    // M = floor64(L - 5K - V - 24) >= 64 and 32 + 8 + K + V <= M.
    let cases = [
        // The record bound: M = 1920 holds 40 + 64 + 1816 = 1920 bytes.
        (4096, 64, 1816, true),
        (4096, 64, 1817, false),
        // 4096 - 5 * 664 - 24 = 752 gives M = 704 = 40 + 664; with
        // K = 665, 747 still gives M = 704 < 40 + 665.
        (4096, 664, 0, true),
        (4096, 665, 0, false),
        // The record bound at the smallest ladder, M = 64.
        (128, 1, 23, true),
        (128, 1, 24, false),
        (128, 2, 22, true),
        (128, 2, 23, false),
        (256, 12, 44, true),
        (256, 12, 45, false),
        // The class bound: 128 - 40 - 0 - 24 = 64, but one more value byte
        // leaves 63, and M = 0 < 64.
        (128, 8, 0, true),
        (128, 8, 1, false),
        (128, 9, 0, false),
        // 64-byte leaves have no ladder at all.
        (64, 1, 0, false),
    ];
    for (leaf, key, value, valid) in cases {
        let result = config(leaf, 4096, key, value).validate();
        assert_eq!(result.is_ok(), valid, "L={leaf} K={key} V={value}");
        let ladder = SizeClasses::derive(leaf, key, value);
        assert_eq!(ladder.is_ok(), valid, "L={leaf} K={key} V={value}");
    }
}

/// Huge limits are rejected, not overflowed: the ladder rule, with checked
/// arithmetic, runs before the inner-node rule's sums.
#[test]
fn huge_limits_are_rejected_without_overflow() {
    let result = config(4096, 4096, usize::MAX, usize::MAX).validate();
    assert!(matches!(result, Err(TreeError::InvalidConfig(_))));
}

#[test]
fn admission_sweep_lists_changes_from_the_old_rule() {
    let keys = [1, 8, 9, 16, 64, 255];
    let values = [0, 16, 64, 256, 1024, 1800];
    let (mut both, mut neither, mut old_only, mut new_only) = (0, 0, 0, 0);
    for leaf in (128..=4096).step_by(64) {
        for key in keys {
            for value in values {
                let old = old_leaf_rule(leaf, key, value);
                let new = config(leaf, 4096, key, value).validate().is_ok();
                match (old, new) {
                    (true, true) => both += 1,
                    (false, false) => neither += 1,
                    (true, false) => old_only += 1,
                    (false, true) => new_only += 1,
                }
            }
        }
    }
    eprintln!(
        "admission sweep: both {both}, neither {neither}, \
         old rule only {old_only}, new rule only {new_only}"
    );

    // Large values: the ladder accepts what the old rule rejected.
    assert!(!old_leaf_rule(4096, 64, 1800));
    assert_eq!(config(4096, 4096, 64, 1800).validate(), Ok(()));
    // Small leaves: a record must fit the 64-byte class, which is stricter.
    // 192 - 45 - 16 - 24 = 107 gives M = 64 < 40 + 9 + 16.
    assert!(old_leaf_rule(192, 9, 16));
    assert!(matches!(
        config(192, 4096, 9, 16).validate(),
        Err(TreeError::InvalidConfig(_))
    ));
    assert!(old_only > 0 && new_only > 0);
    assert_eq!(both + neither + old_only + new_only, 63 * 36);
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
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    let ChildRef::Leaf(root) = tree.root() else {
        panic!("root must be a leaf");
    };
    let bytes = tree.leaves.page_copy(root).unwrap();
    let page = Page::validate(&bytes).unwrap();
    assert_eq!(page.capacity(), 256);
    assert!(page.is_empty());
    assert_eq!(
        page.fences(),
        Some(Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded
        })
    );
    assert_eq!(tree.leaf_count(), 1);
    assert_eq!(tree.inner_count(), 0);
    assert_eq!(tree.config(), &config(256, 128, 8, 16));

    assert!(matches!(
        Tree::in_memory(config(64, 64, 1, 0)),
        Err(TreeError::InvalidConfig(_))
    ));
}

#[test]
fn store_allocates_distinct_zeroed_disjoint_buffers() {
    let mut store = InnerStore::new(64, ChildRef::Leaf(LeafId::new(0).unwrap()));
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
    assert_eq!(InnerId::from_index(5).raw(), 5);
}

#[test]
#[should_panic(expected = "distinct allocated IDs")]
fn store_pair_rejects_aliasing() {
    let mut store = InnerStore::new(64, ChildRef::Leaf(LeafId::new(0).unwrap()));
    let id = store.alloc();
    store.pair_mut(id, id);
}
