//! Routes taken under the inner read lock, and their validation against
//! later structural changes.

use super::*;

fn small() -> Tree {
    Tree::in_memory(config(256, 128, 8, 16)).unwrap()
}

fn key(index: u32) -> [u8; 4] {
    index.to_be_bytes()
}

/// A one-leaf tree whose flushed base page is full enough that merging one
/// more 16-byte value splits it: 7 * (8 + 4 + 16) = 196 of 208 payload bytes
/// are used. The leaf has no mini-page.
fn nearly_full() -> Tree {
    let tree = small();
    for i in 0..7 {
        tree.put(&key(i), &[1; 16]).unwrap();
    }
    tree.flush().unwrap();
    assert_eq!((tree.height(), tree.leaf_count()), (1, 1));
    assert_eq!(tree.buffered_leaves(), 0);
    tree
}

/// Whether leaf `leaf` holds `target` in its mini-page or its base page.
fn holds(tree: &Tree, leaf: LeafId, target: &[u8]) -> (bool, bool) {
    let has = |page: &Page| page.iter().any(|record| record.key.to_vec() == target);
    let mini = tree
        .leaves
        .mini_copy(leaf)
        .is_some_and(|bytes| has(Page::validate(&bytes).unwrap()));
    let bytes = tree.leaves.page_copy(leaf).unwrap();
    (mini, has(Page::validate(&bytes).unwrap()))
}

#[test]
fn a_route_taken_before_a_split_reports_route_changed() {
    let tree = small();
    let route = tree.route(&key(0));
    assert!(!tree.route_changed(&route));

    let shape = (tree.height(), tree.leaf_count());
    let mut i = 0;
    while (tree.height(), tree.leaf_count()) == shape {
        tree.put(&key(i), &[1; 16]).unwrap();
        i += 1;
    }
    assert!(tree.route_changed(&route), "split after {i} puts");

    let fresh = tree.route(&key(0));
    assert!(!tree.route_changed(&fresh));
}

#[test]
fn a_route_is_unchanged_by_writes_that_do_not_split() {
    let tree = small();
    let route = tree.route(b"a");
    tree.put(b"a", b"1").unwrap();
    tree.put(b"b", b"2").unwrap();
    tree.remove(b"a").unwrap();
    assert_eq!(tree.leaf_count(), 1);
    assert!(!tree.route_changed(&route));
    assert_eq!(tree.route(b"a").leaf, route.leaf);
}

#[test]
fn a_write_through_a_stale_route_lands_in_the_right_half() {
    let tree = nearly_full();
    // `key(100)` sorts after every key in the leaf, so the split moves its
    // range to the new right leaf.
    let target = key(100);
    let stale = tree.route(&target);

    // Puts buffer until the mini-page needs a merge; that merge does not fit
    // the full base page, so it splits the leaf.
    let mut i = 7;
    while tree.leaf_count() == 1 {
        assert!(i < 100, "no split after {i} puts");
        tree.put(&key(i), &[1; 16]).unwrap();
        i += 1;
    }
    assert_eq!(tree.leaf_count(), 2, "a put's merge split the leaf");
    assert!(tree.route_changed(&stale));
    assert_ne!(tree.find_leaf(&target), stale.leaf);

    tree.put(&target, b"v").unwrap();
    assert_eq!(tree.get(&target), Ok(Some(b"v".to_vec())));
    assert_eq!(tree.check_invariants(), Ok(()));

    // The record is buffered in the right leaf, not the leaf the stale route
    // named.
    let right = tree.find_leaf(&target);
    assert_ne!(right, stale.leaf);
    assert_eq!(holds(&tree, right, &target), (true, false));
    assert_eq!(holds(&tree, stale.leaf, &target), (false, false));

    // The merge moves it to the right leaf's base page.
    tree.flush().unwrap();
    assert_eq!(holds(&tree, right, &target), (false, true));
    assert_eq!(holds(&tree, stale.leaf, &target), (false, false));
    assert_eq!(tree.get(&target), Ok(Some(b"v".to_vec())));
    assert_eq!(tree.check_invariants(), Ok(()));
}
