use std::marker::PhantomData;

use crate::{InnerId, LeafId};

/// A typed node ID that indexes a [`Store`].
pub(super) trait NodeId: Copy {
    fn from_index(index: usize) -> Self;
    fn index(self) -> usize;
}

impl NodeId for InnerId {
    fn from_index(index: usize) -> Self {
        Self::new(index as u64).expect("store indices stay below the reserved ID")
    }

    fn index(self) -> usize {
        usize::try_from(self.raw()).expect("store IDs index a Vec")
    }
}

impl NodeId for LeafId {
    fn from_index(index: usize) -> Self {
        Self::new(index as u64).expect("store indices stay below the reserved ID")
    }

    fn index(self) -> usize {
        usize::try_from(self.raw()).expect("store IDs index a Vec")
    }
}

// TODO: Replace the leaf store with a page-store boundary behind `LeafId`.
/// Fixed-capacity, zero-initialized byte buffers addressed by a typed ID.
///
/// IDs are dense indices and are never reused, because this single-threaded
/// store never frees a node.
#[derive(Debug)]
pub(super) struct Store<Id> {
    capacity: usize,
    nodes: Vec<Box<[u8]>>,
    _id: PhantomData<Id>,
}

#[allow(dead_code, reason = "the read and write paths arrive in later diffs")]
impl<Id> Store<Id>
where
    Id: NodeId,
{
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            nodes: Vec::new(),
            _id: PhantomData,
        }
    }

    pub(super) fn alloc(&mut self) -> Id {
        let id = Id::from_index(self.nodes.len());
        self.nodes.push(vec![0; self.capacity].into_boxed_slice());

        id
    }

    /// Number of allocated nodes; IDs are `0..len()`.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Borrow an allocated buffer. Panics on an ID this store did not allocate.
    pub(super) fn get(&self, id: Id) -> &[u8] {
        &self.nodes[id.index()]
    }

    pub(super) fn get_mut(&mut self, id: Id) -> &mut [u8] {
        &mut self.nodes[id.index()]
    }

    /// Borrow two allocated buffers mutably at once. Panics if the IDs are equal.
    pub(super) fn pair_mut(&mut self, a: Id, b: Id) -> (&mut [u8], &mut [u8]) {
        let [a, b] = self
            .nodes
            .get_disjoint_mut([a.index(), b.index()])
            .expect("distinct allocated IDs");

        (&mut a[..], &mut b[..])
    }
}
