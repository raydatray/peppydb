//! Building blocks for the BF-Tree storage engine.

pub mod page;

pub use page::{Page, PageError, RecordKind, RecordRef, RecordState};
