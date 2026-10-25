use crate::InnerId;

/// Fixed-capacity, zero-initialized inner-node buffers addressed by
/// [`InnerId`].
///
/// IDs are dense indices and are never reused, because this single-threaded
/// store never frees a node. Leaves live in the
/// [`LeafMap`](super::leaf_map::LeafMap) instead.
#[derive(Debug)]
pub(super) struct InnerStore {
    capacity: usize,
    nodes: Vec<Box<[u8]>>,
}

impl InnerStore {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            nodes: Vec::new(),
        }
    }

    pub(super) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(super) fn alloc(&mut self) -> InnerId {
        let id = InnerId::from_index(self.nodes.len());
        self.nodes.push(vec![0; self.capacity].into_boxed_slice());

        id
    }

    /// Number of allocated nodes; IDs are `0..len()`.
    pub(super) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Borrow an allocated buffer. Panics on an ID this store did not allocate.
    pub(super) fn get(&self, id: InnerId) -> &[u8] {
        &self.nodes[id.index()]
    }

    pub(super) fn get_mut(&mut self, id: InnerId) -> &mut [u8] {
        &mut self.nodes[id.index()]
    }

    /// Borrow two allocated buffers mutably at once. Panics if the IDs are equal.
    pub(super) fn pair_mut(&mut self, a: InnerId, b: InnerId) -> (&mut [u8], &mut [u8]) {
        let [a, b] = self
            .nodes
            .get_disjoint_mut([a.index(), b.index()])
            .expect("distinct allocated IDs");

        (&mut a[..], &mut b[..])
    }
}
