use super::{Tree, TreeError};
use crate::{ChildRef, InnerId, LeafId, RecordState};

/// Inner nodes visited from the root to a leaf, each with the child index
/// that the descent took. The path holds IDs, never node references, so a
/// writer can borrow each parent mutably in turn.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "the write path arrives in the next diff")
)]
pub(super) type Path = Vec<(InnerId, usize)>;

impl Tree {
    /// Return the value stored for `key`, or `None`.
    ///
    /// Rejects an empty key and a key longer than the configured maximum.
    pub fn get(&self, key: &[u8]) -> Result<Option<&[u8]>, TreeError> {
        self.check_key(key)?;

        let record = self.leaf(self.find_leaf(key)).get(key)?;
        Ok(record.and_then(|record| match record.state {
            RecordState::Insert(value) => Some(value),
            _ => None,
        }))
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
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the write path arrives in the next diff")
    )]
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
