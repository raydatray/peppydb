use super::*;
use crate::store::Path;
use bf_tree_node_id::InnerId;

mod config;
mod disk;
mod model;
mod open;
mod read;
mod route;
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
    let store = MemStore::new(Superblock::from(&config)).unwrap();
    Tree::assemble(
        config,
        InnerStore::new(
            config.inner_capacity,
            ChildRef::Leaf(LeafId::new(0).unwrap()),
        ),
        LeafMap::new(Box::new(store), []),
    )
}

/// Test aids over the routing layer.
impl Tree {
    /// Route `key` to its leaf and record the path, ignoring concurrency.
    fn locate(&self, key: &[u8]) -> (LeafId, Path) {
        self.inner.read().locate(key)
    }

    fn root(&self) -> ChildRef {
        self.inner.read().root
    }

    fn set_root(&mut self, root: ChildRef) {
        self.inner.get_mut().root = root;
    }
}

/// Collect a scan's records as owned key/value pairs; any error panics.
fn collect(scan: Scan<'_>) -> Vec<(Vec<u8>, Vec<u8>)> {
    scan.map(Result::unwrap).collect()
}

fn add_leaf(
    tree: &mut Tree,
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
    keys: &[&[u8]],
) -> ChildRef {
    let id = tree.leaves.alloc_id().unwrap();
    let fences = Fences {
        lower: lower.map_or(Fence::Unbounded, Fence::Key),
        upper: upper.map_or(Fence::Unbounded, Fence::Key),
    };
    let mut bytes = vec![0; tree.config.leaf_capacity];
    let page = Page::init_with_fences(&mut bytes, fences).unwrap();
    for key in keys {
        page.try_insert(key, &value_of(key)).unwrap();
    }
    tree.leaves.overwrite(id, &mut bytes).unwrap();
    ChildRef::Leaf(id)
}

fn add_inner(tree: &mut Tree, first: ChildRef, entries: &[(&[u8], ChildRef)]) -> ChildRef {
    let inner = tree.inner.get_mut();
    let id = inner.alloc();
    let node = InnerNode::init(inner.get_mut(id), first).unwrap();
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
    let root = add_inner(&mut tree, l0, &[(b"f", l1), (b"m", l2)]);
    tree.set_root(root);
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
    let root = add_inner(&mut tree, left, &[(b"m", right)]);
    tree.set_root(root);
    tree
}
