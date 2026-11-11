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
use super::frames::{FrameWrite, Frames, Image, Retry, Snapshot};
use bf_tree_inner::{InnerError, InnerNode};
use bf_tree_node_id::{ChildRef, InnerId, LeafId};
use bf_tree_sync::atomic::{AtomicU64, Ordering};
use bf_tree_sync::{Mutex, MutexGuard};

/// Inner nodes visited from the root to a leaf, each with the child index
/// that the descent took.
#[cfg(test)]
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
    /// Held while the root word is replaced. Readers never take it.
    root_grow: Mutex<()>,
}

/// The locked parent of a node whose split is being propagated.
enum Parent<'a> {
    Frame(InnerId, FrameWrite<'a>),
    /// The node is the root; the guard is `root_grow`.
    Root(MutexGuard<'a, ()>),
}

impl Inner {
    pub(super) fn new(capacity: usize, root: ChildRef) -> Self {
        Self {
            root: AtomicU64::new(pack(root)),
            frames: Frames::new(capacity),
            root_grow: Mutex::new(()),
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
    #[cfg(test)]
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
    /// Frames are locked bottom-up, each found by a lock-free descent with
    /// the separator and confirmed under its lock, and published top-down: a
    /// new root word first, then each frame from the highest to the lowest,
    /// image before version. Fresh frames are complete before anything
    /// names them, and are never locked.
    pub(super) fn install(
        &self,
        left: LeafId,
        separator: Vec<u8>,
        right: LeafId,
    ) -> Result<(), TreeError> {
        let capacity = self.capacity();
        let (mut separator, mut left, mut right) =
            (separator, ChildRef::Leaf(left), ChildRef::Leaf(right));
        let mut locked = Vec::new(); // bottom-up
        let mut grown = None; // the root_grow guard and the new root
        loop {
            match self.lock_parent(&separator, left) {
                Parent::Root(guard) => {
                    let root = self.alloc_with(|bytes| {
                        InnerNode::init(bytes, left)?.try_insert(&separator, right)
                    })?;
                    grown = Some((guard, root));
                    break;
                }
                Parent::Frame(id, mut frame) => {
                    match frame.node_mut().try_insert(&separator, right) {
                        Ok(()) => {
                            locked.push(frame);
                            break;
                        }
                        Err(InnerError::NodeFull) => {}
                        Err(error) => return Err(error.into()),
                    }
                    let mut half = Image::new();
                    let bytes = Arc::get_mut(&mut half).expect("a fresh image is unshared");
                    separator = frame
                        .node_mut()
                        .split_with_insert(&mut bytes.bytes_mut()[..capacity], &separator, right)?
                        .separator;
                    right = ChildRef::Inner(self.frames.alloc(half));
                    left = ChildRef::Inner(id);
                    locked.push(frame);
                }
            }
        }
        if let Some((_, root)) = &grown {
            self.root
                .store(pack(ChildRef::Inner(*root)), Ordering::Release);
        }
        // Publish from the highest frame down, then release root_grow.
        while let Some(frame) = locked.pop() {
            drop(frame);
        }
        drop(grown);
        Ok(())
    }

    /// Lock the current parent of `child`, which the caller holds locked
    /// (a leaf's X lock, or a frame's write lock), or `root_grow` if `child`
    /// is the root. `separator` lies in `child`'s range, which the caller's
    /// lock keeps fixed, so every consistent descent with it reaches `child`.
    fn lock_parent(&self, separator: &[u8], child: ChildRef) -> Parent<'_> {
        let mut attempt = 0;
        loop {
            match self.find_parent(separator, child) {
                Ok(None) => {
                    let guard = self.root_grow.lock();
                    // Only the holder of the root's lock replaces the root.
                    if self.root() == child {
                        return Parent::Root(guard);
                    }
                }
                Ok(Some(id)) => {
                    if let Some(frame) = self.confirm(id, separator, child) {
                        return Parent::Frame(id, frame);
                    }
                }
                Err(Retry) => {}
            }
            bf_tree_sync::spin(&mut attempt);
        }
    }

    /// Lock frame `id` and keep it only if it still routes `separator` to
    /// `child`. A concurrent split may have moved `child` to a new frame
    /// between the descent and the lock; the frame is then released
    /// unchanged, which restores its version.
    pub(super) fn confirm(
        &self,
        id: InnerId,
        separator: &[u8],
        child: ChildRef,
    ) -> Option<FrameWrite<'_>> {
        let frame = self.frames.get(id).write();
        (frame.node().route(separator).child == child).then_some(frame)
    }

    /// The frame whose route for `separator` names `child`, by a validated
    /// lock-free descent, or `None` if `child` is the root.
    pub(super) fn find_parent(
        &self,
        separator: &[u8],
        child: ChildRef,
    ) -> Result<Option<InnerId>, Retry> {
        let word = self.root.load(Ordering::Acquire);
        let mut anchor = Anchor::Root(word);
        let mut current = unpack(word);
        if current == child {
            return Ok(None);
        }
        loop {
            let ChildRef::Inner(id) = current else {
                // A different leaf: the descent raced with a publication.
                return Err(Retry);
            };
            let (version, route) = self.frames.get(id).read(|node| node.route(separator))?;
            self.check(anchor)?;
            if route.child == child {
                return Ok(Some(id));
            }
            anchor = Anchor::Parent(id, version);
            current = route.child;
        }
    }
}
