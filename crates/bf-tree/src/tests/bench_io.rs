//! Informational persistence and circular-buffer benchmarks, not gates
//! (plan 0023). Run in release mode, on Linux for contended and direct-I/O
//! numbers (`tools/linux-run`, which sets `PEPPYDB_BENCH_DIR` to an ext4
//! volume):
//!
//! ```text
//! tools/buck2 run @build/mode/release //crates/bf-tree:bf-tree-unittest -- --ignored --nocapture tests::bench_io::
//! ```
//!
//! The settings are those of `tests::bench` (`PEPPYDB_BENCH_KEY`,
//! `PEPPYDB_BENCH_VALUE`, `PEPPYDB_BENCH_CIRCULAR_BUFFER_MIB`,
//! `PEPPYDB_BENCH_KEYS`, `PEPPYDB_BENCH_MILLIS`). Store files go in
//! `PEPPYDB_BENCH_DIR`, or in the system temporary directory. `Io::Direct`
//! parts print "skipped" where the platform or file system refuses
//! `O_DIRECT`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bf_tree_store::FileStore;

use super::bench::{GET, PUT, Rng, Shape, hot_keys, measure, thread_counts};
use super::*;

fn bench_dir() -> tempfile::TempDir {
    match std::env::var_os("PEPPYDB_BENCH_DIR") {
        Some(base) => tempfile::tempdir_in(base).unwrap(),
        None => tempfile::tempdir().unwrap(),
    }
}

/// A new tree over a file store at `path`, or `None` if `io` is refused.
fn create(path: &Path, config: TreeConfig, io: Io) -> Option<Tree> {
    match FileStore::create_with(path, Superblock::from(&config), io) {
        Ok(store) => Some(Tree::create(config, Box::new(store)).unwrap()),
        Err(StoreError::DirectIo(reason)) => {
            println!("  {io:?}: skipped ({reason})");
            None
        }
        Err(StoreError::Io {
            kind: std::io::ErrorKind::InvalidInput,
            ..
        }) => {
            println!("  {io:?}: skipped (the file system refuses O_DIRECT)");
            None
        }
        Err(error) => panic!("{error:?}"),
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1 << 20)
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// Seconds that `f` takes, with its result.
fn timed<R>(f: impl FnOnce() -> R) -> (f64, R) {
    let started = Instant::now();
    let result = f();
    (started.elapsed().as_secs_f64(), result)
}

/// Load, full and partial checkpoints, file size, open rate, and random
/// gets from a circular buffer smaller than the data, per I/O mode.
#[test]
#[ignore = "informational benchmark"]
fn bench_persistence() {
    let shape = Shape::from_env();
    println!("bench_persistence: {}", shape.describe());
    let config = shape.config(true);
    let value = vec![7; shape.value];
    for io in [Io::Cached, Io::Direct] {
        let dir = bench_dir();
        let path = dir.path().join("tree.bf");
        let Some(tree) = create(&path, config, io) else {
            continue;
        };
        let (load, ()) = timed(|| {
            (0..shape.keys).for_each(|i| tree.put(&shape.key(i), &value).unwrap());
        });
        let (full, _) = timed(|| tree.checkpoint().unwrap());
        let after_full = file_len(&path);
        let dirty = hot_keys(&tree, &shape, tree.leaf_count() / 10);
        dirty
            .iter()
            .for_each(|&i| tree.put(&shape.key(i), &value).unwrap());
        let (partial, _) = timed(|| tree.checkpoint().unwrap());
        let after_partial = file_len(&path);
        let leaves = tree.leaf_count();
        drop(tree);

        let (open, tree) = timed(|| {
            let store = FileStore::open_with(&path, io).unwrap();
            Tree::open_with(config, Box::new(store)).unwrap()
        });
        let pages = after_partial / config.leaf_capacity as u64;
        let keys: Vec<_> = (0..shape.keys).collect();
        let gets: Vec<_> = [1, *thread_counts().last().unwrap()]
            .into_iter()
            .map(|threads| {
                let rate = measure(&tree, &shape, &keys, threads, GET);
                format!("{threads}t {rate:.2}")
            })
            .collect();
        println!(
            "  {io:?}: load {:.2} M puts/s; checkpoint of {leaves} leaves {:.0} ms, \
             of 10% ({} keys) {:.0} ms; file {:.1} MiB after the first, \
             {:.1} MiB after the second; open {:.0} ms ({:.0} pages/s); \
             random get {} M ops/s",
            shape.keys as f64 / load / 1e6,
            full * 1e3,
            dirty.len(),
            partial * 1e3,
            mib(after_full),
            mib(after_partial),
            open * 1e3,
            pages as f64 / open,
            gets.join(", "),
        );
    }
}

/// Per-thread put latencies of `threads` threads of replacing puts for the
/// shape's measurement time; returns the rate and sorted latencies.
fn put_latencies(tree: &Tree, shape: &Shape, threads: usize) -> (f64, Vec<u64>) {
    let stop = AtomicBool::new(false);
    let value = vec![9; shape.value];
    let started = Instant::now();
    let mut latencies: Vec<u64> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                let (stop, value) = (&stop, &value);
                scope.spawn(move || {
                    let mut rng = Rng(0x5bd1_e995 ^ ((t as u64 + 1) * 0x2545_f491_4f6c_dd1d));
                    let mut latencies = Vec::with_capacity(1 << 20);
                    while !stop.load(Ordering::Relaxed) {
                        let key = shape.key(rng.below(shape.keys));
                        let started = Instant::now();
                        tree.put(&key, value).unwrap();
                        latencies.push(started.elapsed().as_nanos() as u64);
                    }
                    latencies
                })
            })
            .collect();
        std::thread::sleep(shape.measure);
        stop.store(true, Ordering::Relaxed);
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect()
    });
    let rate = latencies.len() as f64 / started.elapsed().as_secs_f64();
    latencies.sort_unstable();
    (rate, latencies)
}

/// `p50 … µs, p99 … µs, max … µs` of sorted nanosecond latencies.
fn percentiles(sorted: &[u64]) -> String {
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize] as f64 / 1e3;
    format!(
        "p50 {:.1} µs, p99 {:.1} µs, max {:.0} µs",
        at(0.5),
        at(0.99),
        at(1.0)
    )
}

/// Replacing puts into a store file larger than the circular buffer, so
/// that puts evict and write back pages; rate and latency per I/O mode.
#[test]
#[ignore = "informational benchmark"]
fn bench_eviction_io() {
    let shape = Shape::from_env();
    let config = TreeConfig {
        circular_buffer_bytes: 4 << 20,
        ..shape.config(true)
    };
    println!(
        "bench_eviction_io: {}, with a 4 MiB circular buffer",
        shape.describe()
    );
    let value = vec![7; shape.value];
    for io in [Io::Cached, Io::Direct] {
        let dir = bench_dir();
        let path = dir.path().join("tree.bf");
        let Some(tree) = create(&path, config, io) else {
            continue;
        };
        (0..shape.keys).for_each(|i| tree.put(&shape.key(i), &value).unwrap());
        tree.checkpoint().unwrap();
        println!("  {io:?}: store file {:.1} MiB", mib(file_len(&path)));
        for threads in [1, *thread_counts().last().unwrap()] {
            let before = tree.leaves.circular_buffer.metrics().retired;
            let (rate, latencies) = put_latencies(&tree, &shape, threads);
            let evictions = tree.leaves.circular_buffer.metrics().retired - before;
            println!(
                "    {threads}t: {:.2} M puts/s, {}, {:.1} evictions per 1000 puts",
                rate / 1e6,
                percentiles(&latencies),
                evictions as f64 * 1e3 / latencies.len() as f64,
            );
        }
    }
}

/// Put scaling with a 32 MiB circular buffer, where puts evict, against a
/// 1 GiB one, where they rarely do. The gap at high thread counts bounds
/// the cost of the circular-buffer mutex on the eviction path. A `perf`
/// profile would attribute it directly; the container cannot run one.
#[test]
#[ignore = "informational benchmark"]
fn bench_circular_buffer_lock() {
    let shape = Shape::from_env();
    println!("bench_circular_buffer_lock: {}", shape.describe());
    let keys: Vec<_> = (0..shape.keys).collect();
    for buffer_mib in [32, 1024] {
        let shape = Shape {
            buffer_mib,
            ..shape
        };
        let tree = Tree::in_memory(shape.config(true)).unwrap();
        let value = vec![7; shape.value];
        (0..shape.keys).for_each(|i| tree.put(&shape.key(i), &value).unwrap());
        let before = tree.leaves.circular_buffer.metrics().retired;
        let rates: Vec<_> = thread_counts()
            .into_iter()
            .map(|threads| {
                let rate = measure(&tree, &shape, &keys, threads, PUT);
                format!("{threads}t {rate:.2}")
            })
            .collect();
        let evictions = tree.leaves.circular_buffer.metrics().retired - before;
        println!(
            "  {buffer_mib:>4} MiB: put {} M ops/s ({evictions} evictions)",
            rates.join(", ")
        );
    }
}

/// Puts on all threads under a small circular buffer, with and without one
/// eviction stalled for 20 ms every 100 ms while it holds the head claim
/// and its leaf lock. Reports the put rate, evictions per 1000 puts, and
/// latency. If evictions per put jump with the stall, threads that find no
/// room evict younger blocks instead of waiting for the head (plan 0014's
/// open question on a `Condvar` wait).
#[test]
#[ignore = "informational benchmark"]
fn bench_head_stall() {
    let shape = Shape {
        buffer_mib: 4,
        ..Shape::from_env()
    };
    println!("bench_head_stall: {}", shape.describe());
    let tree = Tree::in_memory(shape.config(true)).unwrap();
    let value = vec![7; shape.value];
    (0..shape.keys).for_each(|i| tree.put(&shape.key(i), &value).unwrap());
    let threads = *thread_counts().last().unwrap();
    for stall in [false, true] {
        let done = AtomicBool::new(false);
        let before = tree.leaves.circular_buffer.metrics().retired;
        let (rate, latencies) = std::thread::scope(|scope| {
            if stall {
                scope.spawn(|| {
                    while !done.load(Ordering::Relaxed) {
                        crate::evict::EVICT_STALL_MICROS.store(20_000, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    crate::evict::EVICT_STALL_MICROS.store(0, Ordering::Relaxed);
                });
            }
            let result = put_latencies(&tree, &shape, threads);
            done.store(true, Ordering::Relaxed);
            result
        });
        let evictions = tree.leaves.circular_buffer.metrics().retired - before;
        println!(
            "  {threads}t, {}: {:.2} M puts/s, {:.1} evictions per 1000 puts, {}",
            if stall { "stalled head" } else { "no stall" },
            rate / 1e6,
            evictions as f64 * 1e3 / latencies.len() as f64,
            percentiles(&latencies),
        );
    }
}
