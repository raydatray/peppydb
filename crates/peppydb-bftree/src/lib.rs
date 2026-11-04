//! Building blocks for the BF-Tree storage engine.

pub mod inner;
pub mod store;
pub mod tree;

// Page types that appear in the tree's public API.
pub use bf_tree_page::{PageError, UpsertOutcome};
pub use store::{FileStore, MemStore, PageStore, StoreError, Superblock};
pub use tree::{Scan, Tree, TreeConfig, TreeError};
