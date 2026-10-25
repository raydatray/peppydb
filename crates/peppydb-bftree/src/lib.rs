//! Building blocks for the BF-Tree storage engine.

pub mod inner;
pub mod node_id;
pub mod page;

pub use node_id::{ChildRef, InnerId, LeafId};
pub use page::{
    Fence, Fences, KeyRef, LeafSplit, Page, PageError, RecordKind, RecordRef, RecordState,
    UpsertOutcome,
};
