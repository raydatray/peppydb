//! A BF-Tree of fenced base leaves over a page store.
//!
//! The tree routes through [`InnerNode`]s to fenced leaf [`Page`]s. It starts
//! as one unbounded leaf; the first leaf split creates the first inner node.
//! Every leaf is at the same depth and has its own reader-writer lock. Writes
//! buffer blindly into the leaf's in-memory mini-page, which merges into the
//! base page in the store when full or on `flush`.
//!
//! ```
//! use bf_tree::{Tree, TreeConfig};
//!
//! let tree = Tree::in_memory(TreeConfig::default()).unwrap();
//! for i in 0..1000u32 {
//!     tree.put(&i.to_be_bytes(), b"value").unwrap();
//! }
//! tree.put(&7u32.to_be_bytes(), b"new").unwrap();
//! assert_eq!(tree.get(&7u32.to_be_bytes()), Ok(Some(b"new".to_vec())));
//! tree.remove(&8u32.to_be_bytes()).unwrap();
//! assert_eq!(tree.get(&8u32.to_be_bytes()), Ok(None));
//! assert!(tree.height() > 1);
//!
//! // Scans yield owned `Result`s, because moving to the next leaf may load it.
//! let keys: Vec<_> = tree
//!     .range(Some(&5u32.to_be_bytes()), Some(&10u32.to_be_bytes()))
//!     .unwrap()
//!     .map(|record| record.map(|(key, _)| key))
//!     .collect::<Result<_, _>>()
//!     .unwrap();
//! assert_eq!(keys.len(), 4);
//! assert_eq!(tree.iter().count(), 999);
//! ```

use bf_tree_circular_buffer::CircularBuffer;
use bf_tree_inner::{HEADER_BYTES, InnerNode, entry_size};
use bf_tree_mini_page::SizeClasses;
use bf_tree_node_id::ChildRef;
use bf_tree_page::{Fence, Fences, Page};

// Types from the internal crates that appear in this crate's public API, so
// that callers need no other dependency. Other items stay internal.
pub use bf_tree_inner::InnerError;
pub use bf_tree_node_id::LeafId;
pub use bf_tree_page::PageError;
pub use bf_tree_store::{
    FileStore, MemStore, PageNumber, PageStore, State, StoreError, Superblock,
};

mod evict;
mod frames;
mod inner;
#[cfg(test)]
mod invariants;
mod leaf_map;
mod maintain;
mod open;
mod policy;
mod read;
mod scan;
mod write;

use inner::Inner;
#[cfg(test)]
pub use invariants::InvariantError;
use leaf_map::{LeafMap, Locked, Residency, with_scratch};
use policy::Policy;
pub use scan::Scan;

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
    /// Circular-buffer budget for mini-pages, in bytes. A power of two, at
    /// least `2 * (leaf_capacity + 16)`. A runtime setting: the superblock
    /// does not record it.
    pub circular_buffer_bytes: usize,
    /// Share of the circular buffer, in percent, nearest the head in which
    /// an accessed block is copied to the tail instead of being left for
    /// eviction. 0 disables relocation; values near 100 relocate on almost
    /// every access and tombstone almost every freed block. A runtime
    /// setting.
    pub second_chance_percent: u8,
    /// A write-driven merge keeps the merged page in the circular buffer as
    /// a full page, if one fits without eviction. A runtime setting.
    pub write_load_full_page: bool,
    /// Chance, in percent, that a read served by the base page leaves
    /// something behind: a cached record, or the whole page. Default 20.
    pub read_promotion_percent: u8,
    /// Chance, in percent, that a scan over a leaf with no buffered state
    /// keeps its page in the circular buffer. Default 20.
    pub scan_promotion_percent: u8,
    /// A promoting read caches the record (`true`) or keeps the whole page.
    /// Default `true`.
    pub read_record_cache: bool,
    /// Seed of the promotion rolls.
    pub policy_seed: u64,
}

/// The default second-chance share, 10 percent, as upstream.
pub const DEFAULT_SECOND_CHANCE_PERCENT: u8 = 10;

/// The default circular-buffer budget, 32 MiB.
pub const DEFAULT_CIRCULAR_BUFFER_BYTES: usize = 32 << 20;

impl Default for TreeConfig {
    /// 4096-byte nodes, 64-byte keys, 1024-byte values, a 32 MiB circular
    /// buffer, and a 10 percent second-chance region.
    fn default() -> Self {
        Self {
            leaf_capacity: 4096,
            inner_capacity: 4096,
            max_key_len: 64,
            max_value_len: 1024,
            circular_buffer_bytes: DEFAULT_CIRCULAR_BUFFER_BYTES,
            second_chance_percent: DEFAULT_SECOND_CHANCE_PERCENT,
            write_load_full_page: true,
            read_promotion_percent: 20,
            scan_promotion_percent: 20,
            read_record_cache: true,
            policy_seed: 0x5eed,
        }
    }
}

impl TreeConfig {
    /// Check the capacities and the admission rule that lets every split succeed.
    ///
    /// With leaf capacity `L`, inner capacity `I`, key limit `K`, and value limit
    /// `V`, the rule requires a mini-page ladder, whose largest class
    /// `M = floor64(L - 5K - V - 24)` must be at least 64 and hold a maximum
    /// record (`32 + 8 + K + V <= M`); every leaf split comes from merging a
    /// mini-page, and this `M` makes one two-way split always absorb that
    /// merge. Inner nodes need `16 + max(K - 4, 0) <= (I - 16) / 2`, and the
    /// circular-buffer budget must be valid.
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

        // The ladder bounds K and V by the leaf capacity, with checked
        // arithmetic, so it runs before the inner-node rule's sums.
        let classes = self.size_classes()?;

        if entry_size(self.max_key_len) > (self.inner_capacity - HEADER_BYTES) / 2 {
            return Err(TreeError::InvalidConfig(
                "inner capacity must hold two maximum-length separators",
            ));
        }

        if self.read_promotion_percent > 100 {
            return Err(TreeError::InvalidConfig(
                "read_promotion_percent must be at most 100",
            ));
        }
        if self.scan_promotion_percent > 100 {
            return Err(TreeError::InvalidConfig(
                "scan_promotion_percent must be at most 100",
            ));
        }
        if self.second_chance_percent > 100 {
            return Err(TreeError::InvalidConfig(
                "second_chance_percent must be at most 100",
            ));
        }
        CircularBuffer::new(
            self.circular_buffer_bytes,
            &self.buffer_classes(&classes),
            0,
        )
        .map_err(|_| {
            TreeError::InvalidConfig(
                "circular_buffer_bytes must be a power of two of at least 2 * (leaf_capacity + 16)",
            )
        })?;

        Ok(())
    }

    /// The mini-page size-class ladder these limits allow.
    pub(crate) fn size_classes(&self) -> Result<SizeClasses, TreeError> {
        SizeClasses::derive(self.leaf_capacity, self.max_key_len, self.max_value_len)
            .map_err(|error| TreeError::InvalidConfig(error.0))
    }

    /// The circular buffer's classes: the mini-page ladder, then the leaf
    /// page size for full-page blocks (plan 0015).
    pub(crate) fn buffer_classes(&self, classes: &SizeClasses) -> Vec<usize> {
        let mut all = classes.as_slice().to_vec();
        if all.last() != Some(&self.leaf_capacity) {
            all.push(self.leaf_capacity);
        }
        all
    }
}

/// The superblock a store must carry to hold leaves of this configuration.
///
/// These are `From` conversions, not inherent methods on `Superblock`, so that
/// they can live with `TreeConfig` when the store has its own crate.
impl From<&TreeConfig> for Superblock {
    fn from(config: &TreeConfig) -> Self {
        Self {
            page_size: config.leaf_capacity,
            inner_capacity: config.inner_capacity,
            max_key_len: config.max_key_len,
            max_value_len: config.max_value_len,
        }
    }
}

/// The configuration recorded in a superblock.
/// Runtime settings, such as the circular-buffer budget, take their defaults.
impl From<&Superblock> for TreeConfig {
    fn from(superblock: &Superblock) -> Self {
        Self {
            leaf_capacity: superblock.page_size,
            inner_capacity: superblock.inner_capacity,
            max_key_len: superblock.max_key_len,
            max_value_len: superblock.max_value_len,
            circular_buffer_bytes: DEFAULT_CIRCULAR_BUFFER_BYTES,
            second_chance_percent: DEFAULT_SECOND_CHANCE_PERCENT,
            write_load_full_page: true,
            read_promotion_percent: 20,
            scan_promotion_percent: 20,
            read_record_cache: true,
            policy_seed: 0x5eed,
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
    /// The store cannot hold this tree: its superblock or contents disagree
    /// with the requested operation.
    #[error("invalid store: {0}")]
    InvalidStore(&'static str),
    /// A leaf reached by routing is not an allocated, live leaf. This
    /// indicates a tree bug or an inconsistent store.
    #[error("leaf {} is not allocated", .0.raw())]
    UnknownLeaf(LeafId),
    /// Stored leaf bytes failed validation when loaded.
    #[error("leaf {} is corrupt: {reason}", id.raw())]
    CorruptLeaf { id: LeafId, reason: &'static str },
    /// A stored page failed its checksum or validation on open.
    #[error("page {} is corrupt: {reason}", page_number.raw())]
    CorruptPage {
        page_number: PageNumber,
        reason: &'static str,
    },
    /// The tree has as many leaves as a stored page's 32-bit leaf stamp can
    /// name.
    #[error("the tree has the most leaves a page header can name")]
    TooManyLeaves,
    /// The stored leaves do not tile the key space or exceed the stored
    /// configuration, so no tree can be rebuilt from them.
    #[error("inconsistent leaves: {0}")]
    InconsistentLeaves(&'static str),
    /// A leaf operation failed unexpectedly. This indicates a tree bug.
    #[error("internal leaf error: {0}")]
    Page(#[from] PageError),
    /// An inner-node operation failed unexpectedly. This indicates a tree bug.
    #[error("internal inner-node error: {0}")]
    Inner(#[from] InnerError),
}

/// An ordered map from nonempty byte keys to byte values, shared between
/// threads.
///
/// Leaves live in a [`PageStore`] behind a leaf mapping table with one
/// reader-writer lock per leaf; inner nodes are in-memory data derived from
/// leaf fences, in version-locked frames that readers never lock. Every
/// operation takes `&self`.
///
/// Lock order (plan 0016): a thread acquires waitable locks only in this
/// order, so no cycle of waits can form.
///
/// 1. Leaf locks: at most one reachable leaf, plus a split's fresh right
///    leaf, which no other thread can reach.
/// 2. Frame write locks, bottom-up by level, taken only by
///    `Inner::install` while the thread holds the node whose split it
///    propagates. Fresh frames are never locked.
/// 3. `root_grow`, only while holding the current root's lock.
/// 4. Store locks: a store's page free list, which may in turn take the
///    locks below.
/// 5. Innermost locks: the circular-buffer mutex, a memory store page's
///    mutex, and table growth. Nothing is acquired while one is held.
///
/// Readers take no frame lock and no `root_grow`, and hold at most one leaf
/// S lock. Scans release a leaf before locking the next; eviction and
/// `make_room` run with no leaf lock.
#[derive(Debug)]
pub struct Tree {
    config: TreeConfig,
    inner: Inner,
    leaves: LeafMap,
    policy: Policy,
}

// Compile-time check: a tree and its scans can be shared between threads.
const _: fn() = || {
    fn shared<T: Send + Sync>() {}
    shared::<Tree>();
    shared::<Scan<'static>>();
};

impl Tree {
    /// Validate `config` and create an empty tree on a [`MemStore`]: one
    /// unbounded leaf, nothing persisted.
    ///
    /// This is for tests, scratch state, and other non-durable use. A
    /// durable tree takes a store through [`Tree::create`].
    pub fn in_memory(config: TreeConfig) -> Result<Self, TreeError> {
        config.validate()?;

        let store = MemStore::new(Superblock::from(&config))?;
        Self::with_store(config, Box::new(store))
    }

    /// Validate `config` and create an empty tree on an empty `store` whose
    /// superblock was built from the same configuration.
    ///
    /// The first leaf is written to the store at once; [`Tree::flush`] makes
    /// it durable.
    pub fn create(config: TreeConfig, store: Box<dyn PageStore>) -> Result<Self, TreeError> {
        config.validate()?;
        if *store.superblock() != Superblock::from(&config) {
            return Err(TreeError::InvalidStore(
                "store superblock differs from the configuration",
            ));
        }
        if store.page_count() != 0 || store.state() != State::default() {
            return Err(TreeError::InvalidStore("store already holds pages"));
        }

        Self::with_store(config, store)
    }

    /// Merge every buffered mini-page into its base page and make every
    /// completed write durable.
    ///
    /// Every write that returned before `flush` began is in the store when
    /// it returns. `flush` locks one leaf at a time, so writers may run
    /// meanwhile; a record buffered into a leaf that `flush` already visited
    /// may stay buffered. Dropping a tree, or [`Tree::into_store`], loses
    /// buffered records, so call `flush` first. A process stop between the
    /// two page writes of a split can leave overlapping fences, which
    /// [`Tree::open`] rejects (until shadow paging, plan 0018).
    pub fn flush(&self) -> Result<(), TreeError> {
        for id in self.leaves.ids() {
            let Some(mut leaf) = self.leaves.write_if_live(id) else {
                continue;
            };
            match leaf.residency() {
                // A split's right guard drops before the left guard.
                Residency::Mini { .. } => {
                    with_scratch(|scratch| self.merge_leaf(&mut leaf, scratch).map(drop))?;
                }
                Residency::Full(_) => self.leaves.write_back(&mut leaf)?,
                Residency::Base => {}
            }
        }
        self.leaves.sync()
    }

    /// Number of leaves holding a mini-page: an O(leaves) diagnostic.
    pub fn buffered_leaves(&self) -> usize {
        self.leaves.buffered()
    }

    /// Bytes in use in the circular buffer, dead blocks the head has not
    /// passed included.
    pub fn circular_buffer_used(&self) -> usize {
        self.leaves.circular_buffer.used()
    }

    /// Number of inner nodes.
    pub fn inner_count(&self) -> usize {
        self.inner.len()
    }

    /// Number of leaf IDs, live leaves and holes.
    pub fn leaf_count(&self) -> usize {
        self.leaves.len()
    }

    /// The store that holds the leaves.
    pub fn store(&self) -> &dyn PageStore {
        self.leaves.store()
    }

    /// Close the tree and return its store, for example to reopen it with
    /// [`Tree::open`]. Buffered records are lost; call [`Tree::flush`] first
    /// to keep them and make them durable.
    pub fn into_store(self) -> Box<dyn PageStore> {
        self.leaves.into_store()
    }

    /// Build an empty tree on an empty store that already describes `config`.
    fn with_store(config: TreeConfig, store: Box<dyn PageStore>) -> Result<Self, TreeError> {
        let leaves = LeafMap::for_config(store, [], &config)?;
        let unbounded = Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        };
        let root = {
            let leaf = leaves.alloc()?;
            let mut bytes = vec![0; config.leaf_capacity];
            Page::init_with_fences(&mut bytes, unbounded)?;
            leaves.write_through(&mut [(leaf.id(), &mut bytes)])?;
            leaf.id()
        };

        Ok(Self::assemble(
            config,
            Inner::new(config.inner_capacity, ChildRef::Leaf(root)),
            leaves,
        ))
    }

    fn assemble(config: TreeConfig, inner: Inner, leaves: LeafMap) -> Self {
        Self {
            config,
            inner,
            leaves,
            policy: Policy::new(config.policy_seed),
        }
    }

    /// The validated configuration.
    pub fn config(&self) -> &TreeConfig {
        &self.config
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
}

#[cfg(test)]
mod tests;
