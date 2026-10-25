//! A single-threaded, in-memory BF-Tree of fenced base leaves.
//!
//! The tree routes through [`InnerNode`]s to fenced leaf [`Page`]s. It starts
//! as one unbounded leaf; the first leaf split creates the first inner node.
//! Every leaf is at the same depth.

use crate::inner::{self, InnerError, InnerNode};
use crate::{ChildRef, Fence, Fences, InnerId, LeafId, Page, PageError};

mod store;

use store::Store;

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
    /// A leaf operation failed unexpectedly. This indicates a tree bug.
    #[error("internal leaf error: {0}")]
    Page(#[from] PageError),
    /// An inner-node operation failed unexpectedly. This indicates a tree bug.
    #[error("internal inner-node error: {0}")]
    Inner(#[from] InnerError),
}

/// An ordered map from nonempty byte keys to byte values.
#[derive(Debug)]
#[allow(dead_code, reason = "the read and write paths arrive in later diffs")]
pub struct Tree {
    config: TreeConfig,
    root: ChildRef,
    inner: Store<InnerId>,
    leaves: Store<LeafId>,
}

impl Tree {
    /// Validate `config` and create an empty tree: one unbounded leaf.
    pub fn new(config: TreeConfig) -> Result<Self, TreeError> {
        config.validate()?;

        let mut leaves = Store::new(config.leaf_capacity);
        let root = leaves.alloc();
        let unbounded = Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        };

        Page::init_with_fences(leaves.get_mut(root), unbounded)?;

        Ok(Self {
            config,
            root: ChildRef::Leaf(root),
            inner: Store::new(config.inner_capacity),
            leaves,
        })
    }

    /// The validated configuration.
    pub fn config(&self) -> &TreeConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests;
