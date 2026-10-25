use super::*;

mod config;
mod disk;
mod model;
mod open;
mod read;
mod scan;
mod store;
mod write;

fn config(
    leaf_capacity: usize,
    inner_capacity: usize,
    max_key_len: usize,
    max_value_len: usize,
) -> TreeConfig {
    TreeConfig {
        leaf_capacity,
        inner_capacity,
        max_key_len,
        max_value_len,
    }
}

/// The value hand-built fixtures store for `key`.
fn value_of(key: &[u8]) -> Vec<u8> {
    key.iter().rev().copied().chain(*b"!").collect()
}

/// A tree with no nodes; fixtures allocate nodes and set the root.
fn empty_tree() -> Tree {
    let config = config(256, 128, 8, 16);
    let store = MemStore::new(Superblock::from_config(&config)).unwrap();
    Tree {
        config,
        root: ChildRef::Leaf(LeafId::new(0).unwrap()),
        inner: InnerStore::new(config.inner_capacity),
        leaves: LeafMap::new(Box::new(store), []),
    }
}

/// Collect a scan's records as owned key/value pairs; any error panics.
fn collect(scan: Scan<'_>) -> Vec<(Vec<u8>, Vec<u8>)> {
    scan.map(|record| {
        let record = record.unwrap();
        (record.key.to_vec(), record.state.value().unwrap().to_vec())
    })
    .collect()
}

fn add_leaf(
    tree: &mut Tree,
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
    keys: &[&[u8]],
) -> ChildRef {
    let id = tree.leaves.alloc().unwrap();
    let fences = Fences {
        lower: lower.map_or(Fence::Unbounded, Fence::Key),
        upper: upper.map_or(Fence::Unbounded, Fence::Key),
    };
    let page = Page::init_with_fences(tree.leaves.bytes_mut(id).unwrap(), fences).unwrap();
    for key in keys {
        page.try_insert(key, &value_of(key)).unwrap();
    }
    ChildRef::Leaf(id)
}

fn add_inner(tree: &mut Tree, first: ChildRef, entries: &[(&[u8], ChildRef)]) -> ChildRef {
    let id = tree.inner.alloc();
    let node = InnerNode::init(tree.inner.get_mut(id), first).unwrap();
    for &(key, child) in entries {
        node.try_insert(key, child).unwrap();
    }
    ChildRef::Inner(id)
}

/// Root separators `f` and `m` over three leaves.
fn two_level() -> Tree {
    let mut tree = empty_tree();
    let l0 = add_leaf(&mut tree, None, Some(b"f"), &[b"a", b"c", b"e"]);
    let l1 = add_leaf(&mut tree, Some(b"f"), Some(b"m"), &[b"f", b"h"]);
    let l2 = add_leaf(&mut tree, Some(b"m"), None, &[b"m", b"q", b"z"]);
    tree.root = add_inner(&mut tree, l0, &[(b"f", l1), (b"m", l2)]);
    tree
}

/// Root separator `m` over inner nodes `[f]` and `[t]` and four leaves.
fn three_level() -> Tree {
    let mut tree = empty_tree();
    let l0 = add_leaf(&mut tree, None, Some(b"f"), &[b"a", b"e"]);
    let l1 = add_leaf(&mut tree, Some(b"f"), Some(b"m"), &[b"f", b"lz"]);
    let l2 = add_leaf(&mut tree, Some(b"m"), Some(b"t"), &[b"m", b"s"]);
    let l3 = add_leaf(&mut tree, Some(b"t"), None, &[b"t", b"zz"]);
    let left = add_inner(&mut tree, l0, &[(b"f", l1)]);
    let right = add_inner(&mut tree, l2, &[(b"t", l3)]);
    tree.root = add_inner(&mut tree, left, &[(b"m", right)]);
    tree
}
