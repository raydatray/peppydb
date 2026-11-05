use std::sync::atomic::Ordering;

use super::leaf_map::{LeafWrite, Locked, Residency, with_scratch};
use super::{Tree, TreeError};
use bf_tree_mini_page::Lookup;
use bf_tree_node_id::LeafId;
use bf_tree_page::RecordState;

/// A leaf chosen by routing, and the routing version it was chosen under.
#[derive(Clone, Copy, Debug)]
pub(super) struct Route {
    pub(super) leaf: LeafId,
    version: u64,
}

impl Tree {
    /// Return a copy of the value stored for `key`, or `None`.
    ///
    /// Rejects an empty key and a key longer than the configured maximum.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, TreeError> {
        self.get_with(key, |value| value.map(<[u8]>::to_vec))
    }

    /// Call `f` exactly once with the value stored for `key`, or `None`, and
    /// return its result. The value is borrowed only for the call, so `f` is
    /// the zero-copy form of [`Tree::get`]. `f` runs under the leaf's lock and
    /// must not call back into the tree.
    pub fn get_with<R>(
        &self,
        key: &[u8],
        f: impl FnOnce(Option<&[u8]>) -> R,
    ) -> Result<R, TreeError> {
        self.check_key(key)?;

        let n = self.config.leaf_capacity;
        let leaf = self.read_leaf(key)?;
        // The mini-page answers first; the base page only on a miss.
        if let Residency::Mini { page, refs } = leaf.residency() {
            match page.lookup_marking(refs, key) {
                Lookup::Found(value) => return Ok(f(Some(value))),
                Lookup::Absent => return Ok(f(None)),
                Lookup::Miss => {}
            }
        }
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
        self.inner.read().height()
    }

    /// Route `key` to its leaf under the inner read lock, and note the
    /// version. The lock is released on return.
    pub(super) fn route(&self, key: &[u8]) -> Route {
        let inner = self.inner.read();
        // Written only under the write lock, so stable while we hold read.
        let version = self.inner_version.load(Ordering::Relaxed);
        Route {
            leaf: inner.find_leaf(key),
            version,
        }
    }

    /// Whether a structural change happened since `route` was taken. Called
    /// after locking the routed leaf: a split of that leaf bumps the version
    /// before the splitter releases the leaf's lock.
    pub(super) fn route_changed(&self, route: &Route) -> bool {
        self.inner_version.load(Ordering::Acquire) != route.version
    }

    /// Lock the leaf that owns `key` exclusively and run `f` on it: route,
    /// lock, validate the route, and retry if a split intervened.
    pub(super) fn with_leaf_write<R>(
        &self,
        key: &[u8],
        f: impl FnOnce(&mut LeafWrite<'_>) -> Result<R, TreeError>,
    ) -> Result<R, TreeError> {
        loop {
            let route = self.route(key);
            let mut leaf = self.leaves.write(route.leaf)?;
            if self.route_changed(&route) {
                continue;
            }
            return f(&mut leaf);
        }
    }

    /// Route `key` to its leaf; a test aid that ignores concurrency.
    #[cfg(test)]
    pub(super) fn find_leaf(&self, key: &[u8]) -> LeafId {
        self.inner.read().find_leaf(key)
    }
}
