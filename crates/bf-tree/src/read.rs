use super::inner::Anchor;
use super::leaf_map::{LeafWrite, Locked, Residency, with_scratch};
use super::{Tree, TreeError};
use bf_tree_mini_page::Lookup;
use bf_tree_node_id::LeafId;
use bf_tree_page::RecordState;

/// A leaf chosen by routing, and what validates the route.
#[derive(Clone, Copy, Debug)]
pub(super) struct Route {
    pub(super) leaf: LeafId,
    pub(super) anchor: Anchor,
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
        let noted = self.note(&leaf);
        // The mini-page answers first; the base page only on a miss.
        let buffered = match leaf.residency() {
            Residency::Mini { page, refs } => match page.lookup_marking(refs, key) {
                Lookup::Found(value) => Some(Some(value)),
                Lookup::Absent => Some(None),
                Lookup::Miss => None,
            },
            // A full page is the complete leaf.
            // The route was validated under the leaf lock, so the key is in
            // the leaf's fences.
            Residency::Full(page) => {
                Some(page.get_routed(key).and_then(|record| record.state.value()))
            }
            Residency::Base => None,
        };
        let mut hint = None;
        let result = match buffered {
            Some(value) => f(value),
            None => with_scratch(|scratch| -> Result<_, TreeError> {
                let page = self.leaves.load_base(&leaf, &mut scratch.page[..n])?;
                let value = page.get_routed(key).and_then(|record| match record.state {
                    RecordState::Insert(value) => Some(value),
                    _ => None,
                });
                // A read served by the base page may leave something behind.
                if self.roll_read() {
                    hint = Some(self.read_hint(key, value));
                }
                Ok(f(value))
            })?,
        };
        drop(leaf);
        self.maintain(noted.with_hint(hint))?;
        Ok(result)
    }

    /// Number of levels, counting the leaf level: a single-leaf tree has
    /// height 1.
    pub fn height(&self) -> usize {
        self.inner.height()
    }

    /// Route `key` to its leaf without taking a lock, and note what
    /// validates the route.
    pub(super) fn route(&self, key: &[u8]) -> Route {
        let (leaf, anchor) = self.inner.descend(key);
        Route { leaf, anchor }
    }

    /// Whether the route may be stale. Called after locking the routed leaf:
    /// a split of that leaf publishes its new parent image (or root word)
    /// before the splitter releases the leaf's lock.
    pub(super) fn route_changed(&self, route: &Route) -> bool {
        self.inner.check(route.anchor).is_err()
    }

    /// Lock the leaf that owns `key` exclusively and run `f` on it: route,
    /// lock, validate the route, and retry if a split intervened.
    pub(super) fn with_leaf_write<'t, R>(
        &'t self,
        key: &[u8],
        f: impl FnOnce(&mut LeafWrite<'t>) -> Result<R, TreeError>,
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
        self.inner.descend(key).0
    }
}
