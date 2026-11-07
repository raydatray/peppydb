//! Blind writes against the circular buffer: a concurrent flush, and the
//! blocks a dropped tree returns.

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

#[test]
fn writes_and_a_concurrent_flush_keep_every_pre_flush_write() {
    // Writers on disjoint key ranges, one thread flushing in a loop. After
    // the writers finish, one last flush and a reopen must show every write.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.bf");
    let config = TreeConfig {
        circular_buffer_bytes: 1 << 22,
        ..config(256, 128, 8, 16)
    };
    let store = FileStore::create(&path, Superblock::from(&config)).unwrap();
    let tree = Tree::create(config, Box::new(store)).unwrap();
    let stop = AtomicBool::new(false);

    let expected = std::thread::scope(|scope| {
        let flusher = scope.spawn(|| {
            let mut flushes = 0;
            while !stop.load(Ordering::Relaxed) {
                tree.checkpoint().unwrap();
                flushes += 1;
            }
            flushes
        });
        let writers: Vec<_> = (0..4u8)
            .map(|id| {
                let tree = &tree;
                scope.spawn(move || {
                    let mut written = std::collections::BTreeMap::new();
                    for n in 0..600u16 {
                        let key = [id, (n >> 8) as u8, n as u8];
                        if n % 7 == 3 {
                            tree.remove(&key).unwrap();
                            written.remove(key.as_slice());
                        } else {
                            let value = [id, n as u8, 0xee];
                            tree.put(&key, &value).unwrap();
                            written.insert(key.to_vec(), value.to_vec());
                        }
                    }
                    written
                })
            })
            .collect();
        let mut expected = std::collections::BTreeMap::new();
        for writer in writers {
            expected.extend(writer.join().unwrap());
        }
        stop.store(true, Ordering::Relaxed);
        assert!(flusher.join().unwrap() > 0);
        expected
    });

    tree.checkpoint().unwrap();
    assert_eq!(tree.buffered_leaves(), 0);
    assert_eq!(tree.check_invariants(), Ok(()));
    drop(tree);
    let reopened = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    let all: Vec<_> = reopened.iter().collect::<Result<_, _>>().unwrap();
    assert_eq!(all, expected.into_iter().collect::<Vec<_>>());
    assert_eq!(reopened.check_invariants(), Ok(()));
}

#[test]
fn dropping_a_tree_with_buffered_records_returns_every_block() {
    // Before the fix, the circular buffer saw outstanding blocks at drop and
    // leaked its slab on purpose. Now the map frees them first.
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    for i in 0..50u8 {
        tree.put(&[i], b"buffered").unwrap();
    }
    assert!(tree.buffered_leaves() > 0);
    let metrics_before = tree.leaves.circular_buffer.metrics();
    assert!(metrics_before.allocs > metrics_before.frees);

    let mut leaves = tree.leaves;
    leaves.release_blocks();
    let metrics = leaves.circular_buffer.metrics();
    assert_eq!(metrics.allocs + metrics.reuses, metrics.frees);
    assert_eq!(leaves.circular_buffer.used(), 0);
}
