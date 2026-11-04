//! Building blocks for the BF-Tree storage engine.

pub mod tree;

// Types from other crates that appear in the tree's public API.
pub use bf_tree_inner::InnerError;
pub use bf_tree_page::{PageError, UpsertOutcome};
pub use bf_tree_store::{FileStore, MemStore, PageStore, StoreError, Superblock};
pub use tree::{Scan, Tree, TreeConfig, TreeError};
