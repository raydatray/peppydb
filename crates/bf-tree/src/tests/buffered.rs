//! Blind writes against the circular buffer: a full buffer, and a concurrent
//! flush.

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

/// A circular buffer of 1 KiB holds only a few 64-byte mini-pages.
fn tight() -> TreeConfig {
    TreeConfig {
        circular_buffer_bytes: 1024,
        ..config(256, 128, 8, 16)
    }
}

/// Keys spread far apart, so each lands in its own leaf after splits.
fn wide_key(i: u16) -> [u8; 2] {
    i.to_be_bytes()
}

#[test]
fn a_full_circular_buffer_changes_nothing_and_a_write_succeeds_after_flush() {
    let tree = Tree::in_memory(tight()).unwrap();
    // Build several leaves, then flush so none holds a mini-page.
    for i in 0..400u16 {
        tree.put(&wide_key(i * 97), &[1; 8]).unwrap();
        if i % 20 == 19 {
            tree.flush().unwrap();
        }
    }
    tree.flush().unwrap();
    assert!(tree.leaf_count() > 8);
    assert_eq!(tree.circular_buffer_used(), 0);

    // Buffer one record in leaf after leaf until the buffer is full.
    let leaves: Vec<_> = (0..tree.leaf_count())
        .filter_map(|index| {
            let id = LeafId::from_index(index);
            let page = tree.leaves.page_copy(id).ok()?;
            let page = bf_tree_page::Page::validate(&page).unwrap();
            match page.fences()?.lower {
                bf_tree_page::Fence::Key(lower) => Some(lower.to_vec()),
                bf_tree_page::Fence::Unbounded => Some(vec![0x00]),
            }
        })
        .collect();
    let mut full = None;
    for lower in &leaves {
        let mut key = lower.clone();
        key.push(0xaa);
        match tree.put(&key, b"buffered") {
            Ok(()) => {}
            Err(TreeError::CircularBufferFull) => {
                full = Some(key);
                break;
            }
            Err(error) => panic!("{error}"),
        }
    }
    let key = full.expect("the buffer filled up");

    // Nothing changed: the key is absent, usage is the same.
    let used = tree.circular_buffer_used();
    assert_eq!(tree.get(&key), Ok(None));
    assert_eq!(tree.put(&key, b"again"), Err(TreeError::CircularBufferFull));
    assert_eq!(tree.circular_buffer_used(), used);
    assert_eq!(tree.check_invariants(), Ok(()));

    // Flushing frees every block; the write then succeeds.
    tree.flush().unwrap();
    assert_eq!(tree.circular_buffer_used(), 0);
    tree.put(&key, b"after flush").unwrap();
    assert_eq!(tree.get(&key), Ok(Some(b"after flush".to_vec())));
}

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
                tree.flush().unwrap();
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

    tree.flush().unwrap();
    assert_eq!(tree.buffered_leaves(), 0);
    assert_eq!(tree.check_invariants(), Ok(()));
    drop(tree);
    let reopened = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    let all: Vec<_> = reopened.iter().collect::<Result<_, _>>().unwrap();
    assert_eq!(all, expected.into_iter().collect::<Vec<_>>());
    assert_eq!(reopened.check_invariants(), Ok(()));
}
