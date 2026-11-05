use super::leaf_map::with_scratch;
use super::{Tree, TreeError};
use bf_tree_node_id::{ChildRef, InnerId, LeafId};
use bf_tree_page::RecordState;

/// Inner nodes visited from the root to a leaf, each with the child index
/// that the descent took. The path holds IDs, never node references, so a
/// writer can borrow each parent mutably in turn.
pub(super) type Path = Vec<(InnerId, usize)>;

impl Tree {
    /// Return a copy of the value stored for `key`, or `None`.
    ///
    /// Rejects an empty key and a key longer than the configured maximum.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, TreeError> {
        self.get_with(key, |value| value.map(<[u8]>::to_vec))
    }

    /// Call `f` exactly once with the value stored for `key`, or `None`, and
    /// return its result. The value is borrowed only for the call, so `f` is
    /// the zero-copy form of [`Tree::get`]. `f` must not call back into the
    /// tree.
    pub fn get_with<R>(
        &self,
        key: &[u8],
        f: impl FnOnce(Option<&[u8]>) -> R,
    ) -> Result<R, TreeError> {
        self.check_key(key)?;

        let n = self.config.leaf_capacity;
        let leaf = self.leaves.read(self.find_leaf(key))?;
        with_scratch(|scratch| {
            let page = self.leaves.load_base(&leaf, &mut scratch.page[..n])?;
            page.check_key(key)?;
            let record = page.get(key)?;
            Ok(f(record.and_then(|record| match record.state {
                RecordState::Insert(value) => Some(value),
                _ => None,
            })))
        })
    }

    /// Number of levels, counting the leaf level: a single-leaf tree has
    /// height 1.
    pub fn height(&self) -> usize {
        let mut height = 1;
        let mut child = self.root;
        while let ChildRef::Inner(id) = child {
            child = self.inner(id).first_child();
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
                    let route = self.inner(id).route(key);
                    visit((id, route.index));
                    child = route.child;
                }
            }
        }
    }
}
