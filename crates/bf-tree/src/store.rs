use zerocopy::FromBytes;

use super::TreeError;
use bf_tree_inner::{InnerError, InnerNode};
use bf_tree_node_id::{ChildRef, InnerId, LeafId};

/// Inner nodes visited from the root to a leaf, each with the child index
/// that the descent took.
pub(super) type Path = Vec<(InnerId, usize)>;

/// The routing layer: fixed-capacity inner-node buffers addressed by
/// [`InnerId`], and the root.
///
/// IDs are dense indices and are never reused, because nodes are never
/// freed. The tree keeps this behind one reader-writer lock; leaves live in
/// the [`LeafMap`](super::leaf_map::LeafMap).
#[derive(Debug)]
pub(super) struct InnerStore {
    capacity: usize,
    nodes: Vec<Box<[u8]>>,
    pub(super) root: ChildRef,
}

impl InnerStore {
    pub(super) fn new(capacity: usize, root: ChildRef) -> Self {
        Self {
            capacity,
            nodes: Vec::new(),
            root,
        }
    }

    pub(super) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(super) fn alloc(&mut self) -> InnerId {
        let id = InnerId::from_index(self.nodes.len());
        self.nodes.push(vec![0; self.capacity].into_boxed_slice());

        id
    }

    /// Number of allocated nodes; IDs are `0..len()`.
    pub(super) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Borrow an allocated buffer. Panics on an ID this store did not allocate.
    pub(super) fn get(&self, id: InnerId) -> &[u8] {
        &self.nodes[id.index()]
    }

    pub(super) fn get_mut(&mut self, id: InnerId) -> &mut [u8] {
        &mut self.nodes[id.index()]
    }

    /// Borrow two allocated buffers mutably at once. Panics if the IDs are equal.
    pub(super) fn pair_mut(&mut self, a: InnerId, b: InnerId) -> (&mut [u8], &mut [u8]) {
        let [a, b] = self
            .nodes
            .get_disjoint_mut([a.index(), b.index()])
            .expect("distinct allocated IDs");

        (&mut a[..], &mut b[..])
    }

    pub(super) fn node(&self, id: InnerId) -> &InnerNode {
        InnerNode::ref_from_bytes(self.get(id)).expect("inner buffers hold a node header")
    }

    fn node_mut(&mut self, id: InnerId) -> &mut InnerNode {
        InnerNode::mut_from_bytes(self.get_mut(id)).expect("inner buffers hold a node header")
    }

    /// Number of levels, counting the leaf level.
    pub(super) fn height(&self) -> usize {
        let mut height = 1;
        let mut child = self.root;
        while let ChildRef::Inner(id) = child {
            child = self.node(id).first_child();
            height += 1;
        }

        height
    }

    /// Descend to the leaf whose fences contain `key`, without a path.
    pub(super) fn find_leaf(&self, key: &[u8]) -> LeafId {
        self.descend(key, |_| {})
    }

    /// Descend to the leaf whose fences contain `key` and record the path.
    pub(super) fn locate(&self, key: &[u8]) -> (LeafId, Path) {
        let mut path = Vec::new();
        let leaf = self.descend(key, |step| path.push(step));

        (leaf, path)
    }

    /// Routing is right-on-equality and leaf fences are lower-inclusive, so
    /// any byte slice reaches the one leaf whose fences contain it.
    fn descend(&self, key: &[u8], mut visit: impl FnMut((InnerId, usize))) -> LeafId {
        let mut child = self.root;
        loop {
            match child {
                ChildRef::Leaf(id) => return id,
                ChildRef::Inner(id) => {
                    let route = self.node(id).route(key);
                    visit((id, route.index));
                    child = route.child;
                }
            }
        }
    }

    /// Insert `separator -> right` into the parent recorded on `path`,
    /// splitting full parents in turn. When the path is exhausted, grow a
    /// new root over the old one.
    pub(super) fn propagate(
        &mut self,
        path: &Path,
        mut separator: Vec<u8>,
        right: LeafId,
    ) -> Result<(), TreeError> {
        let mut child = ChildRef::Leaf(right);

        for &(parent, _) in path.iter().rev() {
            match self.node_mut(parent).try_insert(&separator, child) {
                Err(InnerError::NodeFull) => {}
                result => return Ok(result?),
            }

            let right = self.alloc();
            let (left_bytes, right_bytes) = self.pair_mut(parent, right);
            let node =
                InnerNode::mut_from_bytes(left_bytes).expect("inner buffers hold a node header");
            separator = node
                .split_with_insert(right_bytes, &separator, child)?
                .separator;
            child = ChildRef::Inner(right);
        }

        let old_root = self.root;
        let root = self.alloc();
        let node = InnerNode::init(self.get_mut(root), old_root)?;
        node.try_insert(&separator, child)?;
        self.root = ChildRef::Inner(root);

        Ok(())
    }
}
