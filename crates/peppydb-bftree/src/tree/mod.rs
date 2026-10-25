//! A single-threaded BF-Tree of fenced base leaves over a page store.
//!
//! The tree routes through [`InnerNode`]s to fenced leaf [`Page`]s. It starts
//! as one unbounded leaf; the first leaf split creates the first inner node.
//! Every leaf is at the same depth. Leaves are loaded from the store on first
//! access and kept resident.
//!
//! ```
//! use peppydb_bftree::{Tree, TreeConfig, UpsertOutcome};
//!
//! let mut tree = Tree::in_memory(TreeConfig::default()).unwrap();
//! for i in 0..1000u32 {
//!     tree.put(&i.to_be_bytes(), b"value").unwrap();
//! }
//! assert_eq!(tree.put(&7u32.to_be_bytes(), b"new"), Ok(UpsertOutcome::Replaced));
//! assert_eq!(tree.get(&7u32.to_be_bytes()), Ok(Some(b"new".as_slice())));
//! assert_eq!(tree.remove(&8u32.to_be_bytes()), Ok(true));
//! assert_eq!(tree.get(&8u32.to_be_bytes()), Ok(None));
//! assert!(tree.height() > 1);
//!
//! // Scans yield `Result`s because moving to the next leaf may load it.
//! let keys: Vec<_> = tree
//!     .range(Some(&5u32.to_be_bytes()), Some(&10u32.to_be_bytes()))
//!     .unwrap()
//!     .map(|record| record.map(|record| record.key.to_vec()))
//!     .collect::<Result<_, _>>()
//!     .unwrap();
//! assert_eq!(keys.len(), 4);
//! assert_eq!(tree.iter().count(), 999);
//! ```

use zerocopy::FromBytes;

use crate::inner::{self, InnerError, InnerNode};
use crate::store::{MemStore, PageStore, StoreError, Superblock};
use crate::{ChildRef, Fence, Fences, InnerId, LeafId, Page, PageError};

#[cfg(test)]
mod invariants;
mod leaf_map;
mod read;
mod scan;
mod store;
mod write;

#[cfg(test)]
pub use invariants::InvariantError;
use leaf_map::LeafMap;
pub use scan::Scan;
use store::InnerStore;

/// Node capacities and the key and value limits they must support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeConfig {
    /// Leaf page capacity: a multiple of 64 up to 4096 bytes; the admission
    /// rule requires at least 128.
    pub leaf_capacity: usize,
    /// Inner node capacity, with the same rule.
    pub inner_capacity: usize,
    /// Largest accepted key length; at least 1.
    pub max_key_len: usize,
    /// Largest accepted value length; may be 0.
    pub max_value_len: usize,
}

impl Default for TreeConfig {
    /// 4096-byte nodes, 64-byte keys, and 1024-byte values.
    fn default() -> Self {
        Self {
            leaf_capacity: 4096,
            inner_capacity: 4096,
            max_key_len: 64,
            max_value_len: 1024,
        }
    }
}

impl TreeConfig {
    /// Check the capacities and the admission rule that lets every split succeed.
    ///
    /// With leaf capacity `L`, inner capacity `I`, key limit `K`, and value limit
    /// `V`, the rule requires `7K + 3V <= L - 72` and
    /// `16 + max(K - 4, 0) <= (I - 16) / 2`.
    pub fn validate(&self) -> Result<(), TreeError> {
        Page::check_capacity(self.leaf_capacity)
            .map_err(|_| TreeError::InvalidConfig("leaf capacity is not a valid page capacity"))?;

        InnerNode::check_capacity(self.inner_capacity).map_err(|_| {
            TreeError::InvalidConfig("inner capacity is not a valid inner-node capacity")
        })?;

        if self.max_key_len == 0 {
            return Err(TreeError::InvalidConfig(
                "maximum key length must be at least 1",
            ));
        }

        // The larger split output holds a header, one finite fence pair, half
        // of the old payload plus the pending record, and one more record.
        let leaf_demand = self
            .max_key_len
            .saturating_mul(7)
            .saturating_add(self.max_value_len.saturating_mul(3));
        if leaf_demand.saturating_add(72) > self.leaf_capacity {
            return Err(TreeError::InvalidConfig(
                "leaf capacity must satisfy 7 * max_key_len + 3 * max_value_len <= leaf_capacity - 72",
            ));
        }

        if inner::entry_size(self.max_key_len) > (self.inner_capacity - inner::HEADER_BYTES) / 2 {
            return Err(TreeError::InvalidConfig(
                "inner capacity must hold two maximum-length separators",
            ));
        }

        Ok(())
    }
}

impl Superblock {
    /// The superblock a store must carry to hold leaves of this configuration.
    pub fn from_config(config: &TreeConfig) -> Self {
        Self {
            page_size: config.leaf_capacity,
            inner_capacity: config.inner_capacity,
            max_key_len: config.max_key_len,
            max_value_len: config.max_value_len,
        }
    }

    /// The configuration recorded in a superblock.
    pub fn to_config(&self) -> TreeConfig {
        TreeConfig {
            leaf_capacity: self.page_size,
            inner_capacity: self.inner_capacity,
            max_key_len: self.max_key_len,
            max_value_len: self.max_value_len,
        }
    }
}

/// A rejected tree operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("invalid tree configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("key must not be empty")]
    EmptyKey,
    #[error("key exceeds the configured maximum length")]
    KeyTooLarge,
    #[error("value exceeds the configured maximum length")]
    ValueTooLarge,
    #[error("range start must not exceed its end")]
    InvalidRange,
    /// The page store failed.
    #[error("page store error: {0}")]
    Store(#[from] StoreError),
    /// A leaf reached by routing is not an allocated, live slot. This
    /// indicates a tree bug or an inconsistent store.
    #[error("leaf {} is not allocated", .0.raw())]
    UnknownLeaf(LeafId),
    /// Stored leaf bytes failed validation when loaded.
    #[error("leaf {} is corrupt: {reason}", id.raw())]
    CorruptLeaf { id: LeafId, reason: &'static str },
    /// A leaf operation failed unexpectedly. This indicates a tree bug.
    #[error("internal leaf error: {0}")]
    Page(#[from] PageError),
    /// An inner-node operation failed unexpectedly. This indicates a tree bug.
    #[error("internal inner-node error: {0}")]
    Inner(#[from] InnerError),
}

/// An ordered map from nonempty byte keys to byte values.
///
/// Leaves live in a [`PageStore`] behind a leaf mapping table; inner nodes
/// are in-memory data derived from leaf fences.
#[derive(Debug)]
pub struct Tree {
    config: TreeConfig,
    root: ChildRef,
    inner: InnerStore,
    leaves: LeafMap,
}

impl Tree {
    /// Validate `config` and create an empty tree on a [`MemStore`]: one
    /// unbounded leaf, nothing persisted.
    ///
    /// This is for tests, scratch state, and other non-durable use. A
    /// durable tree takes a store through [`Tree::create`].
    pub fn in_memory(config: TreeConfig) -> Result<Self, TreeError> {
        config.validate()?;

        let store = MemStore::new(Superblock::from_config(&config))?;
        Self::with_store(config, Box::new(store))
    }

    /// Build an empty tree on an empty store that already describes `config`.
    fn with_store(config: TreeConfig, store: Box<dyn PageStore>) -> Result<Self, TreeError> {
        let mut leaves = LeafMap::new(store, []);
        let root = leaves.alloc()?;
        let unbounded = Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        };

        Page::init_with_fences(leaves.bytes_mut(root)?, unbounded)?;

        Ok(Self {
            config,
            root: ChildRef::Leaf(root),
            inner: InnerStore::new(config.inner_capacity),
            leaves,
        })
    }

    /// The validated configuration.
    pub fn config(&self) -> &TreeConfig {
        &self.config
    }

    /// Number of inner nodes.
    pub fn inner_count(&self) -> usize {
        self.inner.len()
    }

    /// Number of leaf slots in the store, live or freed.
    pub fn leaf_count(&self) -> usize {
        self.leaves.len()
    }

    /// Number of leaves currently held in memory.
    pub fn resident_leaves(&self) -> usize {
        self.leaves.resident_count()
    }

    /// Number of leaves whose resident copy is newer than the store.
    pub fn dirty_leaves(&self) -> usize {
        self.leaves.dirty_count()
    }

    /// The store that holds the leaves.
    pub fn store(&self) -> &dyn PageStore {
        self.leaves.store()
    }

    /// Close the tree and return its store, for example to reopen it with
    /// [`Tree::open`]. Unflushed changes are lost; call [`Tree::flush`] first
    /// to keep them.
    pub fn into_store(self) -> Box<dyn PageStore> {
        self.leaves.into_store()
    }

    /// Reject keys outside the configured limits.
    fn check_key(&self, key: &[u8]) -> Result<(), TreeError> {
        if key.is_empty() {
            return Err(TreeError::EmptyKey);
        }
        if key.len() > self.config.max_key_len {
            return Err(TreeError::KeyTooLarge);
        }

        Ok(())
    }

    fn leaf(&self, id: LeafId) -> Result<&Page, TreeError> {
        self.leaves.page(id)
    }

    fn leaf_mut(&mut self, id: LeafId) -> Result<&mut Page, TreeError> {
        self.leaves.page_mut(id)
    }

    fn inner(&self, id: InnerId) -> &InnerNode {
        InnerNode::ref_from_bytes(self.inner.get(id)).expect("inner buffers hold a node header")
    }

    fn inner_mut(&mut self, id: InnerId) -> &mut InnerNode {
        InnerNode::mut_from_bytes(self.inner.get_mut(id)).expect("inner buffers hold a node header")
    }
}

#[cfg(test)]
mod tests;
