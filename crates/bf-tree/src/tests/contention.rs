//! Overlapping keys under contention, across caching policies, stores, and
//! reopens (plan 0016).
//!
//! Several writers write the same keys. A value names its key, its writer,
//! and the writer's sequence number `n`, which increases with every write
//! by that writer. Readers check that each value they see is well formed,
//! and that for one key and one writer, `n` never decreases. Point
//! operations are linearizable per key, so afterwards each key holds the
//! last write of one of its writers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::concurrent::{StopOnDrop, with_watchdog};
use super::*;

/// Keys every writer shares: `[0x10, k_hi, k_lo]`. Few enough that writers
/// collide constantly, spread over enough leaves to split and evict.
const KEYS: u16 = 600;

fn shared_key(k: u16) -> Vec<u8> {
    let [hi, lo] = k.to_be_bytes();
    vec![0x10, hi, lo]
}

/// `[key..., writer, n (4 bytes), pad...]`; the pad bytes equal `n`'s low
/// byte, so a torn or misplaced value is detected.
fn tagged(key: &[u8], writer: u8, n: u32, pad: usize) -> Vec<u8> {
    let mut value = key.to_vec();
    value.push(writer);
    value.extend(n.to_be_bytes());
    value.resize(value.len() + pad, n as u8);
    value
}

/// The writer and sequence number of a well-formed value, or `None`.
fn parse(key: &[u8], value: &[u8]) -> Option<(u8, u32)> {
    let rest = value.strip_prefix(key)?;
    let (&writer, rest) = rest.split_first()?;
    let n = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
    rest[4..]
        .iter()
        .all(|&byte| byte == n as u8)
        .then_some((writer, n))
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

/// Each writer's last sequence number per key: `last[writer][k]`.
type Last = Vec<Vec<Option<u32>>>;

/// One phase: `writers` writers, each doing `ops` puts starting at sequence
/// number `start`, against readers and scanners. Returns each writer's
/// last write per key.
fn phase(tree: &Tree, writers: u8, readers: usize, start: u32, ops: u32) -> Last {
    let max_pad = tree.config().max_value_len - shared_key(0).len() - 5;
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..writers)
            .map(|id| {
                scope.spawn(move || {
                    let mut rng = Rng(u64::from(id) * 0x9e37 + u64::from(start) + 1);
                    let mut last = vec![None; usize::from(KEYS)];
                    for n in start..start + ops {
                        let k = rng.below(u64::from(KEYS)) as u16;
                        let key = shared_key(k);
                        let pad = rng.below(max_pad as u64 + 1) as usize;
                        tree.put(&key, &tagged(&key, id, n, pad)).unwrap();
                        last[usize::from(k)] = Some(n);
                    }
                    last
                })
            })
            .collect();
        let others: Vec<_> = (0..readers)
            .map(|i| {
                let stop = &stop;
                scope.spawn(move || {
                    let mut rng = Rng(i as u64 * 7919 + 17);
                    // The highest `n` seen per key and writer.
                    let mut seen = vec![vec![0u32; usize::from(writers)]; usize::from(KEYS)];
                    let mut checks = 0usize;
                    while !stop.load(Ordering::Relaxed) || checks < 50 {
                        if i % 3 == 2 {
                            // Scanners: order and well-formed values.
                            let start = shared_key(rng.below(u64::from(KEYS)) as u16);
                            let mut previous: Option<Vec<u8>> = None;
                            for record in tree.range(Some(&start), None).unwrap().take(40) {
                                let (key, value) = record.unwrap();
                                assert!(parse(&key, &value).is_some(), "{key:?}");
                                assert!(previous.as_ref().is_none_or(|p| p < &key));
                                previous = Some(key);
                            }
                        } else {
                            let k = rng.below(u64::from(KEYS)) as u16;
                            let key = shared_key(k);
                            if let Some(value) = tree.get(&key).unwrap() {
                                let (writer, n) = parse(&key, &value).expect("well formed");
                                let high = &mut seen[usize::from(k)][usize::from(writer)];
                                assert!(n >= *high, "key {k}: writer {writer} went back");
                                *high = n;
                            }
                        }
                        checks += 1;
                    }
                    checks
                })
            })
            .collect();
        let stopping = StopOnDrop(&stop);
        let last: Last = handles.into_iter().map(|h| h.join().unwrap()).collect();
        drop(stopping);
        for other in others {
            assert!(other.join().unwrap() > 0);
        }
        last
    })
}

/// At quiescence: each written key holds one of its writers' last writes,
/// keys never written are absent, and the invariants hold.
fn check_final(tree: &Tree, last: &[Last]) {
    for k in 0..KEYS {
        let key = shared_key(k);
        let candidates: Vec<(u8, u32)> = (0..last[0].len())
            .filter_map(|writer| {
                // The latest phase that wrote this key decides.
                let n = last
                    .iter()
                    .rev()
                    .find_map(|phase| phase[writer][usize::from(k)])?;
                Some((writer as u8, n))
            })
            .collect();
        match tree.get(&key).unwrap() {
            None => assert!(candidates.is_empty(), "key {k} lost"),
            Some(value) => {
                let found = parse(&key, &value).expect("well formed");
                // Only the latest phase's writes can be final for a key the
                // latest phase wrote.
                let latest = last
                    .iter()
                    .rev()
                    .find(|phase| phase.iter().any(|w| w[usize::from(k)].is_some()))
                    .expect("a written key");
                let finals: Vec<_> = latest
                    .iter()
                    .enumerate()
                    .filter_map(|(w, keys)| keys[usize::from(k)].map(|n| (w as u8, n)))
                    .collect();
                assert!(
                    finals.contains(&found),
                    "key {k}: {found:?} not in {finals:?}"
                );
                assert!(candidates.contains(&found));
            }
        }
    }
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[derive(Clone, Copy, Debug)]
enum Backend {
    Memory,
    File,
}

/// Run two phases with a flush and reopen between them, and check the end
/// state against the writers' last writes.
fn run_phases(config: TreeConfig, backend: Backend, writers: u8, readers: usize, ops: u32) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("contention.bf");
    let superblock = Superblock::from(&config);
    let tree = match backend {
        Backend::Memory => Tree::create(config, Box::new(MemStore::new(superblock).unwrap())),
        Backend::File => Tree::create(
            config,
            Box::new(FileStore::create(&path, superblock).unwrap()),
        ),
    }
    .unwrap();

    let first = phase(&tree, writers, readers, 0, ops);
    check_final(&tree, std::slice::from_ref(&first));
    assert!(tree.circular_buffer_used() <= config.circular_buffer_bytes);

    tree.checkpoint().unwrap();
    let tree = match backend {
        Backend::Memory => Tree::open_with(config, tree.into_store()).unwrap(),
        Backend::File => {
            drop(tree);
            Tree::open_with(config, Box::new(FileStore::open(&path).unwrap())).unwrap()
        }
    };
    check_final(&tree, std::slice::from_ref(&first));

    let second = phase(&tree, writers, readers, ops, ops);
    check_final(&tree, &[first, second]);
    assert!(tree.circular_buffer_used() <= config.circular_buffer_bytes);
}

/// Caching policies: `(read rate, record mode, scan rate, write-load)`.
const POLICIES: [(u8, bool, u8, bool); 3] = [
    (30, true, 30, false), // record caching
    (30, false, 30, true), // page caching and full pages
    (0, true, 0, false),   // no caching
];

fn tiny(bytes: usize, policy: (u8, bool, u8, bool)) -> TreeConfig {
    let (read, record, scan, full) = policy;
    TreeConfig {
        circular_buffer_bytes: bytes,
        read_promotion_percent: read,
        read_record_cache: record,
        scan_promotion_percent: scan,
        write_load_full_page: full,
        second_chance_percent: 10,
        ..config(256, 128, 8, 16)
    }
}

#[test]
fn overlapping_writers_agree_with_their_last_writes() {
    for policy in POLICIES {
        for (bytes, backend) in [(1024, Backend::Memory), (2048, Backend::File)] {
            with_watchdog(Duration::from_secs(120), move || {
                run_phases(tiny(bytes, policy), backend, 6, 6, 1500);
            });
        }
    }
}

#[test]
fn overlapping_writers_on_a_large_buffer_agree() {
    with_watchdog(Duration::from_secs(120), || {
        let config = TreeConfig {
            read_promotion_percent: 50,
            ..config(512, 256, 8, 32)
        };
        run_phases(config, Backend::Memory, 8, 4, 2000);
    });
}

/// A long randomized run for Linux, not a gate. Run with
/// `tools/linux-run tools/buck2 run @build/mode/release //crates/bf-tree:bf-tree-unittest -- --ignored concurrent_soak`;
/// `PEPPYDB_SOAK_SECS` sets the duration (default 60).
#[test]
#[ignore = "soak test"]
fn concurrent_soak() {
    let seconds: u64 = std::env::var("PEPPYDB_SOAK_SECS")
        .ok()
        .map_or(60, |text| text.parse().expect("seconds"));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let threads = std::thread::available_parallelism().map_or(4, usize::from);
    let mut round = 0u64;
    while Instant::now() < deadline {
        let policy = POLICIES[(round % 3) as usize];
        let bytes = [1024, 2048, 4096][(round / 3 % 3) as usize];
        let backend = if round.is_multiple_of(2) {
            Backend::Memory
        } else {
            Backend::File
        };
        let writers = (threads / 2).clamp(2, 16) as u8;
        with_watchdog(Duration::from_secs(300), move || {
            run_phases(tiny(bytes, policy), backend, writers, threads / 2 + 1, 3000);
        });
        round += 1;
    }
    println!("concurrent_soak: {round} rounds in {seconds}s, {threads} threads");
}
