//! The routing layer: inner nodes in version-locked frames, and the root
//! word (plan 0016).
//!
//! Readers take no lock. A descent reads each frame optimistically, reads
//! the child, and then validates the parent: the frame above, or the root
//! word for the top frame. After the caller locks the leaf it routed to, it
//! validates the leaf's parent once more with [`Inner::check`]. A leaf's
//! range shrinks only by its own split, which publishes the new parent image
//! before the splitter releases the leaf, so an unchanged parent means the
//! leaf still owns the key.

use std::sync::Arc;

use super::TreeError;
use super::frames::{Frames, Image, Retry, Snapshot};
use bf_tree_inner::{InnerError, InnerNode};
use bf_tree_node_id::{ChildRef, InnerId, LeafId};
use bf_tree_sync::Mutex;
use bf_tree_sync::atomic::{AtomicU64, Ordering};

/// Inner nodes visited from the root to a leaf, each with the child index
/// that the descent took.
pub(super) type Path = Vec<(InnerId, usize)>;

/// The root word marks a leaf root with this bit; dense IDs never reach it.
const LEAF_ROOT: u64 = 1 << 63;

fn pack(child: ChildRef) -> u64 {
    match child {
        ChildRef::Leaf(id) => LEAF_ROOT | id.raw(),
        ChildRef::Inner(id) => id.raw(),
    }
}

fn unpack(word: u64) -> ChildRef {
    if word & LEAF_ROOT == 0 {
        ChildRef::Inner(InnerId::new(word).expect("packed IDs are valid"))
    } else {
        ChildRef::Leaf(LeafId::new(word & !LEAF_ROOT).expect("packed IDs are valid"))
    }
}

/// What validates a route: the node that named its last child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Anchor {
    /// The root word named the child, with this value. The root word changes
    /// only by growth, to a fresh frame, so it never returns to a value.
    Root(u64),
    /// The frame that named the child, and the version it was read at.
    Parent(InnerId, u64),
}

/// The inner nodes and the root.
#[derive(Debug)]
pub(super) struct Inner {
    root: AtomicU64,
    frames: Frames,
    /// Serializes structural changes until bottom-up frame locking lands
    /// (plan 0016, diff 7). Readers never take it.
    structure: Mutex<()>,
}

impl Inner {
    pub(super) fn new(capacity: usize, root: ChildRef) -> Self {
        Self {
            root: AtomicU64::new(pack(root)),
            frames: Frames::new(capacity),
            structure: Mutex::new(()),
        }
    }

    pub(super) fn capacity(&self) -> usize {
        self.frames.capacity()
    }

    #[cfg(test)]
    pub(super) fn frames(&self) -> &Frames {
        &self.frames
    }

    pub(super) fn root(&self) -> ChildRef {
        unpack(self.root.load(Ordering::Acquire))
    }

    /// Replace the root while no other thread uses the tree: open, and test
    /// fixtures.
    pub(super) fn set_root(&self, root: ChildRef) {
        self.root.store(pack(root), Ordering::Release);
    }

    /// Allocate a frame whose image `build` initializes in place. The frame
    /// is complete before any parent or the root can name it.
    pub(super) fn alloc_with(
        &self,
        build: impl FnOnce(&mut [u8]) -> Result<(), InnerError>,
    ) -> Result<InnerId, InnerError> {
        let mut image = Image::new();
        let bytes = Arc::get_mut(&mut image).expect("a fresh image is unshared");
        build(&mut bytes.bytes_mut()[..self.capacity()])?;
        Ok(self.frames.alloc(image))
    }

    /// Number of allocated nodes; IDs are `0..len()`.
    pub(super) fn len(&self) -> usize {
        self.frames.len()
    }

    /// A node's current image without validation, for quiescent walkers.
    pub(super) fn snapshot(&self, id: InnerId) -> Snapshot {
        self.frames.get(id).snapshot()
    }

    /// Number of levels, counting the leaf level. Exact at quiescence.
    pub(super) fn height(&self) -> usize {
        let mut height = 1;
        let mut child = self.root();
        while let ChildRef::Inner(id) = child {
            child = self.snapshot(id).node().first_child();
            height += 1;
        }
        height
    }

    /// One optimistic descent to the leaf whose fences contain `key`: read
    /// each frame, then validate its parent. `visit` sees each inner node
    /// and the child index taken. `Retry` if a writer interfered.
    pub(super) fn try_descend(
        &self,
        key: &[u8],
        mut visit: impl FnMut(InnerId, usize),
    ) -> Result<(LeafId, Anchor), Retry> {
        let word = self.root.load(Ordering::Acquire);
        let mut anchor = Anchor::Root(word);
        let mut child = unpack(word);
        loop {
            let id = match child {
                ChildRef::Leaf(leaf) => return Ok((leaf, anchor)),
                ChildRef::Inner(id) => id,
            };
            let (version, route) = self.frames.get(id).read(|node| node.route(key))?;
            self.check(anchor)?; // read the child, then validate its parent
            visit(id, route.index);
            anchor = Anchor::Parent(id, version);
            child = route.child;
        }
    }

    /// Descend until a descent validates. Never blocks; retries back off.
    pub(super) fn descend(&self, key: &[u8]) -> (LeafId, Anchor) {
        let mut attempt = 0;
        loop {
            if let Ok(found) = self.try_descend(key, |_, _| {}) {
                return found;
            }
            bf_tree_sync::spin(&mut attempt);
        }
    }

    /// [`Inner::descend`] that also records the path.
    pub(super) fn locate(&self, key: &[u8]) -> (LeafId, Path) {
        let mut attempt = 0;
        let mut path = Vec::new();
        loop {
            path.clear();
            if let Ok((leaf, _)) = self.try_descend(key, |id, index| path.push((id, index))) {
                return (leaf, path);
            }
            bf_tree_sync::spin(&mut attempt);
        }
    }

    /// `Ok` if the node that named a route's last child is unchanged.
    pub(super) fn check(&self, anchor: Anchor) -> Result<(), Retry> {
        match anchor {
            Anchor::Root(word) if self.root.load(Ordering::Acquire) == word => Ok(()),
            Anchor::Root(_) => Err(Retry),
            Anchor::Parent(id, version) => self.frames.get(id).validate(version),
        }
    }

    /// Insert `separator -> right` above the leaf `left`, splitting full
    /// parents and growing the root as needed. The caller holds the
    /// exclusive locks of both leaves and has written both pages through.
    ///
    /// Frames are locked bottom-up and published top-down: a new root word
    /// first, then each frame from the highest to the lowest, image before
    /// version. Fresh frames are complete before anything names them.
    pub(super) fn install(
        &self,
        left: LeafId,
        separator: Vec<u8>,
        right: LeafId,
    ) -> Result<(), TreeError> {
        let _structure = self.structure.lock();
        // Structural changes are serialized, so this descent cannot race with
        // one; the separator lies in `left`'s range, which `left`'s lock
        // keeps fixed.
        let (leaf, path) = self.locate(&separator);
        debug_assert_eq!(leaf, left, "the separator lies in the left leaf's range");

        let capacity = self.capacity();
        let mut separator = separator;
        let mut child = ChildRef::Leaf(right);
        let mut locked = Vec::new(); // bottom-up
        let mut placed = false;
        for &(parent, _) in path.iter().rev() {
            let mut frame = self.frames.get(parent).write();
            match frame.node_mut().try_insert(&separator, child) {
                Ok(()) => {
                    locked.push(frame);
                    placed = true;
                    break;
                }
                Err(InnerError::NodeFull) => {}
                Err(error) => return Err(error.into()),
            }
            let mut half = Image::new();
            let bytes = Arc::get_mut(&mut half).expect("a fresh image is unshared");
            separator = frame
                .node_mut()
                .split_with_insert(&mut bytes.bytes_mut()[..capacity], &separator, child)?
                .separator;
            child = ChildRef::Inner(self.frames.alloc(half));
            locked.push(frame);
        }
        if !placed {
            let old_root = self.root();
            let root = self.alloc_with(|bytes| {
                InnerNode::init(bytes, old_root)?.try_insert(&separator, child)
            })?;
            self.root
                .store(pack(ChildRef::Inner(root)), Ordering::Release);
        }
        // Publish from the highest frame down.
        while let Some(frame) = locked.pop() {
            drop(frame);
        }
        Ok(())
    }
}
