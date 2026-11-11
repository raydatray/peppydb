//! Version-locked frames: optimistic reads, validation, and publication.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::concurrent::with_watchdog;
use crate::frames::{Frames, Image, Retry};
use bf_tree_inner::InnerNode;
use bf_tree_node_id::{ChildRef, LeafId};

const CAPACITY: usize = 128;

fn node_image(first: u64) -> Arc<Image> {
    let mut image = Image::new();
    let bytes = &mut Arc::get_mut(&mut image).unwrap().bytes_mut()[..CAPACITY];
    InnerNode::init(bytes, ChildRef::Leaf(LeafId::new(first).unwrap())).unwrap();
    image
}

fn first(node: &InnerNode) -> ChildRef {
    node.first_child()
}

fn leaf(raw: u64) -> ChildRef {
    ChildRef::Leaf(LeafId::new(raw).unwrap())
}

#[test]
fn a_read_validates_until_a_publication() {
    let frames = Frames::new(CAPACITY);
    assert_eq!(frames.capacity(), CAPACITY);
    let id = frames.alloc(node_image(0));
    let frame = frames.get(id);
    let (version, child) = frame.read(first).unwrap();
    assert_eq!((version, child), (0, leaf(0)));
    assert_eq!(frame.validate(version), Ok(()));

    let mut guard = frame.write();
    guard.node_mut().try_insert(b"m", leaf(1)).unwrap();
    // While held: reads retry, and earlier reads no longer validate.
    assert_eq!(frame.read(first), Err(Retry));
    assert_eq!(frame.validate(version), Err(Retry));
    assert_eq!(guard.node().len(), 1, "the guard sees its draft");
    drop(guard);

    let (after, len) = frame.read(InnerNode::len).unwrap();
    assert_eq!((after, len), (2, 1));
    assert_eq!(frame.validate(version), Err(Retry));
    assert_eq!(frames.len(), 1);
}

#[test]
fn an_unchanged_release_restores_the_version() {
    let frames = Frames::new(CAPACITY);
    let frame = frames.get(frames.alloc(node_image(0)));
    let (version, _) = frame.read(first).unwrap();
    let guard = frame.write();
    assert_eq!(guard.node().first_child(), leaf(0));
    drop(guard);
    assert_eq!(frame.validate(version), Ok(()), "earlier reads stay valid");
    assert_eq!(frame.read(first).unwrap().0, version);
}

#[test]
fn snapshots_see_the_published_image() {
    let frames = Frames::new(CAPACITY);
    let frame = frames.get(frames.alloc(node_image(3)));
    let before = frame.snapshot();
    frame.write().node_mut().try_insert(b"k", leaf(4)).unwrap();
    assert_eq!(before.node().len(), 0, "an old snapshot is immutable");
    assert_eq!(frame.snapshot().node().len(), 1);
}

#[test]
fn writers_exclude_each_other() {
    with_watchdog(Duration::from_secs(60), || {
        let frames = Frames::new(CAPACITY);
        let frame = frames.get(frames.alloc(Image::new()));
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..2000 {
                        let mut guard = frame.write();
                        let bytes = guard.image_mut().bytes_mut();
                        let count = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                        bytes[..8].copy_from_slice(&(count + 1).to_le_bytes());
                    }
                });
            }
        });
        let (version, count) = frame
            .read_image(|image| u64::from_le_bytes(image.bytes()[..8].try_into().unwrap()))
            .unwrap();
        assert_eq!(count, 8000, "no update was lost");
        assert_eq!(version, 16000);
    });
}

/// Writers publish images whose bytes all equal the publishing version / 2;
/// readers check that no image is torn and that a validated read at `v`
/// holds the image published at `v`.
#[test]
fn validated_reads_hold_the_image_of_their_version() {
    with_watchdog(Duration::from_secs(60), || {
        let frames = Frames::new(CAPACITY);
        let frame = frames.get(frames.alloc(Image::new()));
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        let mut validated = 0u64;
                        // After the writers stop, the frame is stable, so
                        // every reader ends with at least one validated read.
                        while !stop.load(Ordering::Relaxed) || validated == 0 {
                            let Ok((version, (low, high))) = frame.read_image(|image| {
                                let bytes = image.bytes();
                                (bytes.iter().min().copied(), bytes.iter().max().copied())
                            }) else {
                                continue;
                            };
                            assert_eq!(low, high, "a torn image");
                            if frame.validate(version).is_ok() {
                                assert_eq!(low, Some((version / 2) as u8));
                                validated += 1;
                            }
                        }
                        validated
                    })
                })
                .collect();
            for _ in 0..2 {
                scope.spawn(|| {
                    for _ in 0..20_000 {
                        let mut guard = frame.write();
                        let next = guard.published_version();
                        guard.image_mut().bytes_mut().fill((next / 2) as u8);
                    }
                });
            }
            // 40,000 publications end at version 80,000; then stop readers.
            while frame.read_image(|_| ()).map_or(true, |(v, _)| v < 80_000) {
                std::thread::yield_now();
            }
            stop.store(true, Ordering::Relaxed);
            for reader in readers {
                assert!(reader.join().unwrap() > 0);
            }
        });
    });
}
