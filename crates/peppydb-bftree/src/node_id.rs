//! Opaque identifiers for tree nodes.
//!
//! IDs are neither addresses nor disk offsets. Their allocation, resolution,
//! uniqueness, and reuse rules belong to the components that own the nodes.
//!
//! ```
//! use peppydb_bftree::{ChildRef, InnerId, LeafId};
//!
//! let leaf = LeafId::new(7).unwrap();
//! assert_eq!(leaf.raw(), 7);
//! assert_eq!(LeafId::new(u64::MAX), None);
//!
//! let child = ChildRef::from(leaf);
//! assert_ne!(child, ChildRef::from(InnerId::new(7).unwrap()));
//! assert_eq!(child.raw(), 7);
//! ```

/// Raw value that no ID may use, matching the page's no-backing-leaf sentinel.
const RESERVED: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct InnerId(u64);

impl InnerId {
    /// Returns `None` for the reserved `u64::MAX`.
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == RESERVED {
            return None;
        }

        Some(Self(raw))
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The ID for a dense table or slot index. Indices never reach the
    /// reserved value, so this cannot fail for any real index.
    pub(crate) fn from_index(index: usize) -> Self {
        Self::new(index as u64).expect("indices stay below the reserved ID")
    }

    /// The dense index this ID addresses.
    pub(crate) fn index(self) -> usize {
        usize::try_from(self.0).expect("IDs index in-memory tables")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct LeafId(u64);

impl LeafId {
    /// Returns `None` for the reserved `u64::MAX`.
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == RESERVED {
            return None;
        }

        Some(Self(raw))
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The ID for a dense table or slot index. Indices never reach the
    /// reserved value, so this cannot fail for any real index.
    pub(crate) fn from_index(index: usize) -> Self {
        Self::new(index as u64).expect("indices stay below the reserved ID")
    }

    /// The dense index this ID addresses.
    pub(crate) fn index(self) -> usize {
        usize::try_from(self.0).expect("IDs index in-memory tables")
    }
}

/// A typed reference from an inner node to one child.
///
/// Equality includes the kind: a leaf and an inner node with the same raw
/// value are different children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChildRef {
    Inner(InnerId),
    Leaf(LeafId),
}

impl ChildRef {
    /// The raw ID value, without the kind.
    pub const fn raw(self) -> u64 {
        match self {
            Self::Inner(id) => id.raw(),
            Self::Leaf(id) => id.raw(),
        }
    }

    pub const fn is_leaf(self) -> bool {
        matches!(self, Self::Leaf(_))
    }
}

impl From<InnerId> for ChildRef {
    fn from(id: InnerId) -> Self {
        Self::Inner(id)
    }
}

impl From<LeafId> for ChildRef {
    fn from(id: LeafId) -> Self {
        Self::Leaf(id)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn accept_every_value_except_the_reserved_maximum() {
        for raw in [0, 1, 4096, u64::MAX / 2, u64::MAX - 1] {
            assert_eq!(InnerId::new(raw).map(InnerId::raw), Some(raw));
            assert_eq!(LeafId::new(raw).map(LeafId::raw), Some(raw));
        }
        assert_eq!(InnerId::new(u64::MAX), None);
        assert_eq!(LeafId::new(u64::MAX), None);
    }

    #[test]
    fn child_references_round_trip_and_keep_their_kind() {
        let inner = InnerId::new(3).unwrap();
        let leaf = LeafId::new(3).unwrap();

        assert_eq!(ChildRef::from(inner), ChildRef::Inner(inner));
        assert_eq!(ChildRef::from(leaf), ChildRef::Leaf(leaf));
        assert_eq!(ChildRef::from(inner).raw(), 3);
        assert_eq!(ChildRef::from(leaf).raw(), 3);
        assert!(ChildRef::from(leaf).is_leaf());
        assert!(!ChildRef::from(inner).is_leaf());

        let distinct: HashSet<_> = [ChildRef::from(inner), ChildRef::from(leaf)].into();
        assert_eq!(distinct.len(), 2);
    }

    #[test]
    fn ids_are_eight_bytes() {
        assert_eq!(size_of::<InnerId>(), 8);
        assert_eq!(size_of::<LeafId>(), 8);
    }
}
