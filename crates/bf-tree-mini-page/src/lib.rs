//! A single leaf's mini-page: the in-memory overlay that buffers writes and
//! caches records above the leaf's base page (BF-Tree paper, §§3.1, 5).
//!
//! A logical leaf is a fenced base page plus at most one mini-page. The
//! mini-page is unfenced, names its leaf in `backing_leaf`, may hold any
//! record kind, and has a capacity from a ladder of size classes. It lives in
//! a block of the circular buffer.

mod class;

pub use class::SizeClasses;

/// Leaf limits from which no size-class ladder can be derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid mini-page limits: {0}")]
pub struct InvalidLimits(pub &'static str);

#[cfg(test)]
mod tests;
