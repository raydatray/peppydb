//! The shared tree under threads, against per-thread oracles.
//!
//! Writers own disjoint key prefixes, so the union of their oracles is the
//! expected tree. Interleaving the prefixes in key order makes writers share
//! leaves and race on splits. Each writer also owns a counter key that only
//! it updates, with increasing values; readers check that a counter never
//! decreases and that every value they see is well formed. Scanners check
//! order. A watchdog fails the test instead of hanging on a deadlock.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::*;

/// A deterministic generator per thread.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// Keys of writer `writer`: `[n_hi, n_lo, writer]`, so writers' keys
/// interleave in key order and share leaves.
fn key(writer: u8, n: u16) -> Vec<u8> {
    let [hi, lo] = n.to_be_bytes();
    vec![hi, lo, writer]
}

/// The counter key of writer `writer`, sorting after every data key.
fn counter(writer: u8) -> Vec<u8> {
    vec![0xff, 0xff, writer]
}

/// A value that names its key, so a reader can check it was not torn or
/// misplaced: `[key..., generation]`, possibly padded.
fn value(key: &[u8], generation: u8, pad: usize) -> Vec<u8> {
    let mut value = key.to_vec();
    value.push(generation);
    value.resize(value.len() + pad, generation);
    value
}

fn well_formed(key: &[u8], value: &[u8]) -> bool {
    value.len() > key.len()
        && value.starts_with(key)
        && value[key.len() + 1..]
            .iter()
            .all(|&b| b == value[key.len()])
}

/// Run `f` on a thread with a deadline; panic if it does not finish, which
/// surfaces a deadlock as a failure instead of a hang.
fn with_watchdog<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, finished) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = f();
        let _ = done.send(());
        result
    });
    match finished.recv_timeout(limit) {
        Ok(()) => worker.join().unwrap(),
        // A panic in the worker drops the sender: surface the panic.
        Err(mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("watchdog: no progress in {limit:?}"),
    }
}

/// One writer: puts and removes on its own keys, mirrored in its oracle, and
/// increasing values on its counter key.
fn writer(tree: &Tree, id: u8, ops: usize, max_value: usize) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut rng = Rng(u64::from(id) * 7919 + 1);
    let mut oracle = BTreeMap::new();
    for step in 0..ops {
        let key = key(id, rng.below(300) as u16);
        if rng.below(5) == 0 {
            tree.remove(&key).unwrap();
            oracle.remove(&key);
        } else {
            let pad = rng.below(max_value - key.len());
            let value = value(&key, (step % 251) as u8, pad);
            tree.put(&key, &value).unwrap();
            oracle.insert(key, value);
        }
        if step % 10 == 0 {
            let count = (step / 10) as u32;
            tree.put(&counter(id), &count.to_be_bytes()).unwrap();
            oracle.insert(counter(id), count.to_be_bytes().to_vec());
        }
    }
    oracle
}

/// One reader: random gets over every writer's keys, checking values are
/// well formed and counters never decrease.
fn reader(tree: &Tree, seed: u64, writers: u8, stop: &AtomicBool) -> usize {
    let mut rng = Rng(seed);
    let mut seen = vec![0u32; usize::from(writers)];
    let mut reads = 0;
    while !stop.load(Ordering::Relaxed) || reads < 100 {
        let writer = rng.below(usize::from(writers)) as u8;
        let key = key(writer, rng.below(300) as u16);
        if let Some(value) = tree.get(&key).unwrap() {
            assert!(well_formed(&key, &value), "{key:?} -> {value:?}");
        }
        if let Some(count) = tree.get(&counter(writer)).unwrap() {
            let count = u32::from_be_bytes(count.try_into().unwrap());
            assert!(count >= seen[usize::from(writer)], "counter went backwards");
            seen[usize::from(writer)] = count;
        }
        reads += 1;
    }
    reads
}

/// One scanner: full and partial scans in both forms, checking order and
/// well-formed values.
fn scanner(tree: &Tree, seed: u64, stop: &AtomicBool) -> usize {
    let mut rng = Rng(seed);
    let mut scans = 0;
    while !stop.load(Ordering::Relaxed) || scans < 5 {
        let start = key(0, rng.below(300) as u16);
        let records: Vec<_> = tree
            .range(Some(&start), None)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            records.windows(2).all(|w| w[0].0 < w[1].0),
            "scan out of order"
        );
        assert!(
            records
                .iter()
                .all(|(key, _)| key.as_slice() >= start.as_slice())
        );
        for (key, value) in &records {
            if key[0] != 0xff {
                assert!(well_formed(key, value));
            }
        }

        let mut previous: Option<Vec<u8>> = None;
        tree.scan_with(None, Some(&start), |key, _| {
            assert!(previous.as_deref().is_none_or(|p| p < key));
            previous = Some(key.to_vec());
            std::ops::ControlFlow::Continue(())
        })
        .unwrap();
        scans += 1;
    }
    scans
}

/// Run writers, readers, and scanners on one tree; afterwards the tree must
/// equal the union of the writers' oracles and pass its invariants.
fn run(tree: Tree, writers: u8, readers: usize, ops: usize) -> Tree {
    run_shared(&tree, writers, readers, ops);
    tree
}

/// [`run`] on a shared tree, so other threads may use it meanwhile.
fn run_shared(tree: &Tree, writers: u8, readers: usize, ops: usize) {
    let max_value = tree.config().max_value_len;
    let stop = AtomicBool::new(false);
    let oracle = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..writers)
            .map(|id| {
                scope.spawn({
                    let tree = &tree;
                    move || writer(tree, id, ops, max_value)
                })
            })
            .collect();
        let others: Vec<_> = (0..readers)
            .map(|i| {
                let (tree, stop) = (&tree, &stop);
                scope.spawn(move || match i % 2 {
                    0 => reader(tree, i as u64 + 100, writers, stop),
                    _ => scanner(tree, i as u64 + 200, stop),
                })
            })
            .collect();

        let mut oracle = BTreeMap::new();
        for handle in handles {
            oracle.extend(handle.join().unwrap());
        }
        stop.store(true, Ordering::Relaxed);
        for handle in others {
            assert!(handle.join().unwrap() > 0);
        }
        oracle
    });

    let all: Vec<_> = tree.iter().collect::<Result<_, _>>().unwrap();
    assert_eq!(all, oracle.into_iter().collect::<Vec<_>>());
    assert_eq!(tree.check_invariants(), Ok(()));
}

/// Tiny nodes force frequent leaf splits and root growth.
fn tiny() -> TreeConfig {
    config(256, 128, 8, 16)
}

#[test]
fn threads_agree_with_the_union_of_their_oracles() {
    for threads in [2u8, 4, 8] {
        with_watchdog(Duration::from_secs(120), move || {
            let tree = Tree::in_memory(tiny()).unwrap();
            let tree = run(tree, threads, usize::from(threads), 1500);
            assert!(
                tree.height() >= 3,
                "{threads} threads: height {}",
                tree.height()
            );
        });
    }
}

#[test]
fn threads_on_a_file_store_survive_flush_and_reopen() {
    with_watchdog(Duration::from_secs(120), || {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tree.bf");
        let config = tiny();
        let store = FileStore::create(&path, Superblock::from(&config)).unwrap();
        let tree = Tree::create(config, Box::new(store)).unwrap();
        let tree = run(tree, 4, 4, 1000);
        tree.flush().unwrap();
        let expected: Vec<_> = tree.iter().collect::<Result<_, _>>().unwrap();
        drop(tree);

        let reopened = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
        let actual: Vec<_> = reopened.iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(actual, expected);
        assert_eq!(reopened.check_invariants(), Ok(()));
    });
}

#[test]
fn many_threads_on_one_leaf_range_make_progress() {
    // Every writer hammers the same few keys of one leaf: maximal contention
    // on one lock and on the splits around it.
    with_watchdog(Duration::from_secs(60), || {
        let tree = Tree::in_memory(tiny()).unwrap();
        let start = Instant::now();
        std::thread::scope(|scope| {
            for id in 0..8u8 {
                let tree = &tree;
                scope.spawn(move || {
                    for n in 0..500u16 {
                        let key = vec![0, (n % 4) as u8, id];
                        tree.put(&key, &value(&key, 1, 3)).unwrap();
                        assert!(well_formed(&key, &tree.get(&key).unwrap().unwrap()));
                    }
                });
            }
        });
        assert_eq!(tree.iter().count(), 4 * 8);
        assert_eq!(tree.check_invariants(), Ok(()));
        assert!(start.elapsed() < Duration::from_secs(60));
    });
}

/// Tiny nodes and a circular buffer of about four leaf pages: writers,
/// readers, scanners, and a looping flush all evict and race on the same
/// leaves.
#[test]
fn threads_agree_under_constant_eviction() {
    for (bytes, percent, full) in [(1024usize, 10u8, true), (2048, 100, true), (1024, 0, false)] {
        with_watchdog(Duration::from_secs(120), move || {
            let config = TreeConfig {
                circular_buffer_bytes: bytes,
                second_chance_percent: percent,
                write_load_full_page: full,
                // Page promotion on reads, where full pages are on.
                read_promotion_percent: if full { 30 } else { 0 },
                read_record_cache: false,
                ..tiny()
            };
            let stop = AtomicBool::new(false);
            let tree = Tree::in_memory(config).unwrap();
            std::thread::scope(|scope| {
                let flusher = scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        tree.flush().unwrap();
                        assert!(tree.circular_buffer_used() <= bytes);
                    }
                });
                run_shared(&tree, 4, 4, 1500);
                stop.store(true, Ordering::Relaxed);
                flusher.join().unwrap();
            });
            assert!(tree.height() >= 3);
            tree.flush().unwrap();
            let expected: Vec<_> = tree.iter().collect::<Result<_, _>>().unwrap();
            let reopened = Tree::open(tree.into_store()).unwrap();
            let actual: Vec<_> = reopened.iter().collect::<Result<_, _>>().unwrap();
            assert_eq!(actual, expected);
        });
    }
}

/// With every live block claimed, a writer that needs room sees `Busy` and
/// waits; once the claims are evicted, it finishes.
#[test]
fn a_writer_waits_while_every_block_is_claimed() {
    with_watchdog(Duration::from_secs(60), || {
        let config = TreeConfig {
            circular_buffer_bytes: 1024,
            ..tiny()
        };
        let tree = Tree::in_memory(config).unwrap();
        // Fill the buffer with blocks of many leaves.
        let mut i = 0u16;
        while tree.leaves.circular_buffer.has_room(200) || tree.leaf_count() < 4 {
            tree.put(&i.wrapping_mul(4099).to_be_bytes(), &[1; 12])
                .unwrap();
            i += 1;
        }
        let mut victims = Vec::new();
        while let bf_tree_circular_buffer::Claim::Victim(victim) =
            tree.leaves.circular_buffer.claim_victim()
        {
            victims.push(victim);
        }
        assert!(!victims.is_empty());
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                // Needs room: a key far from the others, in a big record.
                for n in 0..20u8 {
                    tree.put(&[0xfe, n], &[2; 16]).unwrap();
                }
                done.store(true, Ordering::Relaxed);
            });
            std::thread::sleep(Duration::from_millis(50));
            for victim in victims {
                tree.evict(victim).unwrap();
            }
            writer.join().unwrap();
        });
        assert!(done.load(Ordering::Relaxed));
        assert_eq!(tree.get(&[0xfe, 19]), Ok(Some(vec![2; 16])));
        assert_eq!(tree.check_invariants(), Ok(()));
    });
}
