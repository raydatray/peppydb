//! Version-locked frames: the concurrent home of the inner nodes (plan 0016).
//!
//! A frame is a version word and an immutable image of the node bytes,
//! published through `arc-swap`. Readers take no lock: they read the
//! version, read the image, and later validate that the version did not
//! change (optimistic latch coupling, paper §3.4). A writer takes the
//! version odd with a CAS, edits a private draft, and publishes the draft
//! image before the version becomes even again. A published image is never
//! changed, so a reader can never see a torn node.
//!
//! Memory ordering, with `v` the even version a reader saw:
//!
//! - Writer: CAS `v` to `v + 1` (acquire), `fence(Release)`, store the image,
//!   store `v + 2` (release).
//! - Reader: load the version (acquire), load the image (acquire, by
//!   `arc-swap`), and validate with a second version load (acquire).
//!
//! If the reader's image load returns the new image, the fence makes the CAS
//! visible to it, so its validation sees at least `v + 1` and fails. So an
//! even version that validates names exactly the image published at it.
//! The argument needs the image load to be at least an acquire and the image
//! store at least a release. `arc-swap` 1.9 documents more: every concurrent
//! operation of the default strategy, `load` included, contains at least one
//! `SeqCst` read-modify-write (its `docs` module, "Atomic orderings").

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use bf_tree_inner::InnerNode;
use bf_tree_node_id::InnerId;
use bf_tree_sync::Table;
use bf_tree_sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};
use zerocopy::FromBytes;

/// Every image has this many bytes; a node uses the first `capacity`.
/// `arc-swap` needs a sized `T`, and an inline array saves a pointer chase.
pub(super) const IMAGE_BYTES: usize = 4096;

/// An immutable copy of one node's bytes, once published.
#[derive(Clone)]
pub(super) struct Image {
    bytes: [u8; IMAGE_BYTES],
}

impl Image {
    /// A zeroed image, to be filled before it is published.
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            bytes: [0; IMAGE_BYTES],
        })
    }

    #[cfg(test)]
    pub(super) fn bytes(&self) -> &[u8; IMAGE_BYTES] {
        &self.bytes
    }

    pub(super) fn bytes_mut(&mut self) -> &mut [u8; IMAGE_BYTES] {
        &mut self.bytes
    }

    pub(super) fn node(&self, capacity: usize) -> &InnerNode {
        InnerNode::ref_from_bytes(&self.bytes[..capacity]).expect("images hold a node header")
    }

    pub(super) fn node_mut(&mut self, capacity: usize) -> &mut InnerNode {
        InnerNode::mut_from_bytes(&mut self.bytes[..capacity]).expect("images hold a node header")
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image").finish_non_exhaustive()
    }
}

/// The concurrent home of one inner node.
#[derive(Debug, Default)]
pub(super) struct Frame {
    /// Even: no writer. Odd: a writer holds the frame. A publication adds 2
    /// in total, so an even value names exactly one published image.
    version: AtomicU64,
    /// `None` only in table entries that were never allocated.
    image: ArcSwapOption<Image>,
}

/// A reader followed a stale route; the operation restarts from the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Retry;

/// Every frame of a tree. Frames are never freed, because inner nodes only
/// split, so an `InnerId` never names a different node.
#[derive(Debug)]
pub(super) struct Frames {
    table: Table<Frame>,
    next: AtomicUsize,
    capacity: usize,
}

impl Frames {
    pub(super) fn new(capacity: usize) -> Self {
        assert!(capacity <= IMAGE_BYTES, "inner capacity fits an image");
        Self {
            table: Table::new(),
            next: AtomicUsize::new(0),
            capacity,
        }
    }

    /// The node capacity in bytes.
    pub(super) fn capacity(&self) -> usize {
        self.capacity
    }

    /// A frame that already holds `image`, at version 0. Nothing can reach
    /// it until a published parent image or the root word names it.
    pub(super) fn alloc(&self, image: Arc<Image>) -> InnerId {
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        self.table.grow_to(index + 1);
        let frame = self.table.get(index).expect("grown above");
        frame.image.store(Some(image));
        InnerId::from_index(index)
    }

    /// The frame `id`. Panics on an ID this table did not allocate.
    pub(super) fn get(&self, id: InnerId) -> FrameRef<'_> {
        FrameRef {
            frame: self.table.get(id.index()).expect("allocated frame"),
            capacity: self.capacity,
        }
    }

    /// Number of allocated frames; IDs are `0..len()`.
    pub(super) fn len(&self) -> usize {
        self.next.load(Ordering::Acquire)
    }
}

/// A borrowed frame.
#[derive(Clone, Copy, Debug)]
pub(super) struct FrameRef<'a> {
    frame: &'a Frame,
    capacity: usize,
}

impl<'a> FrameRef<'a> {
    /// Read the current image and the version it is published at, or `Retry`
    /// if a writer holds the frame. The result is trustworthy only after
    /// [`FrameRef::validate`] with the returned version.
    pub(super) fn read_image<R>(self, f: impl FnOnce(&Image) -> R) -> Result<(u64, R), Retry> {
        let version = self.frame.version.load(Ordering::Acquire);
        if version & 1 == 1 {
            return Err(Retry);
        }
        let image = self.frame.image.load(); // after the version
        let image = image.as_ref().expect("reachable frames hold an image");
        Ok((version, f(image)))
    }

    /// [`FrameRef::read_image`] through the node view.
    pub(super) fn read<R>(self, f: impl FnOnce(&InnerNode) -> R) -> Result<(u64, R), Retry> {
        let capacity = self.capacity;
        self.read_image(|image| f(image.node(capacity)))
    }

    /// `Ok` if the frame is still at version `seen`.
    pub(super) fn validate(self, seen: u64) -> Result<(), Retry> {
        if self.frame.version.load(Ordering::Acquire) == seen {
            Ok(())
        } else {
            Err(Retry)
        }
    }

    /// The current image, for walkers that run at quiescence (the invariant
    /// checker, `height`, counts) and therefore need no validation.
    pub(super) fn snapshot(self) -> Snapshot {
        Snapshot {
            image: self
                .frame
                .image
                .load_full()
                .expect("allocated frames hold an image"),
            capacity: self.capacity,
        }
    }

    /// Lock the frame for writing: spin until the version is even and take
    /// it odd. The draft is copied from the current image on first use.
    pub(super) fn write(self) -> FrameWrite<'a> {
        let mut attempt = 0;
        loop {
            let version = self.frame.version.load(Ordering::Relaxed);
            if version & 1 == 0
                && self
                    .frame
                    .version
                    .compare_exchange_weak(
                        version,
                        version + 1,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                // Order the odd version before the image store (module docs).
                fence(Ordering::Release);
                return FrameWrite {
                    frame: self.frame,
                    capacity: self.capacity,
                    locked: version + 1,
                    current: self
                        .frame
                        .image
                        .load_full()
                        .expect("allocated frames hold an image"),
                    draft: None,
                };
            }
            bf_tree_sync::spin(&mut attempt);
        }
    }
}

/// An image read without validation, valid only at quiescence.
#[derive(Debug)]
pub(super) struct Snapshot {
    image: Arc<Image>,
    capacity: usize,
}

impl Snapshot {
    pub(super) fn node(&self) -> &InnerNode {
        self.image.node(self.capacity)
    }

    /// The node's bytes.
    #[cfg(test)]
    pub(super) fn bytes(&self) -> &[u8] {
        &self.image.bytes()[..self.capacity]
    }
}

/// A frame held for writing. Dropping it publishes the draft, if any, and
/// releases the frame; with no draft it restores the old version, which
/// still names the unchanged image.
#[derive(Debug)]
pub(super) struct FrameWrite<'a> {
    frame: &'a Frame,
    capacity: usize,
    /// The odd version this guard set.
    locked: u64,
    /// The image published when the guard was taken; stable while it holds
    /// the frame, because only the holder publishes.
    current: Arc<Image>,
    draft: Option<Arc<Image>>,
}

impl FrameWrite<'_> {
    /// The version that dropping this guard publishes if it has a draft.
    #[cfg(test)]
    pub(super) fn published_version(&self) -> u64 {
        self.locked + 1
    }

    /// The node as this guard sees it: the draft if it changed anything,
    /// else the current image.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by lock_parent from diff 7")
    )]
    pub(super) fn node(&self) -> &InnerNode {
        self.draft
            .as_ref()
            .unwrap_or(&self.current)
            .node(self.capacity)
    }

    /// The draft's node, copied from the current image on first use.
    pub(super) fn node_mut(&mut self) -> &mut InnerNode {
        let capacity = self.capacity;
        self.image_mut().node_mut(capacity)
    }

    /// The draft image, copied from the current image on first use.
    pub(super) fn image_mut(&mut self) -> &mut Image {
        let current = &self.current;
        let draft = self
            .draft
            .get_or_insert_with(|| Arc::new(Image::clone(current)));
        Arc::get_mut(draft).expect("the draft is unshared until published")
    }
}

impl Drop for FrameWrite<'_> {
    fn drop(&mut self) {
        match self.draft.take() {
            Some(draft) => {
                self.frame.image.store(Some(draft)); // image first,
                self.frame.version.store(self.locked + 1, Ordering::Release); // then version
            }
            // Nothing changed: the old version still names the image.
            None => self.frame.version.store(self.locked - 1, Ordering::Release),
        }
    }
}
