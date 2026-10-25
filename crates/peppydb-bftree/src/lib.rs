//! Building blocks for the BF-Tree storage engine.

pub mod node_id;
pub mod page;

pub use node_id::{ChildRef, InnerId, LeafId};
pub use page::{
    Fence, Fences, KeyRef, Page, PageError, RecordKind, RecordRef, RecordState, UpsertOutcome,
};
