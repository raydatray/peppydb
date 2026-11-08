//! Informational multi-threaded benchmarks, not gates (plan 0016). Plan
//! 0016 records them under the interim inner lock and again after frames
//! replace it, from the same harness. Run with
//!
//! ```text
//! tools/buck2 run @build/mode/release //crates/bf-tree:bf-tree-unittest -- --ignored --nocapture tests::bench::
//! ```
//!
//! Contended numbers are meaningful only on Linux (`tools/linux-run`): the
//! macOS `RwLock` parks readers under reader-reader contention. Settings:
//! `PEPPYDB_BENCH_KEY` and `PEPPYDB_BENCH_VALUE` (bytes, default 8 and 16),
//! `PEPPYDB_BENCH_CIRCULAR_BUFFER_MIB` (default 32), `PEPPYDB_BENCH_KEYS`
//! (default 1,000,000), and `PEPPYDB_BENCH_MILLIS` per measurement
//! (default 1000). The benchmarks carry no shared instrumentation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::*;

/// The record shape and run lengths, from the environment.
#[derive(Clone, Copy, Debug)]
pub(super) struct Shape {
    pub(super) key: usize,
    pub(super) value: usize,
    pub(super) buffer_mib: usize,
    pub(super) keys: u64,
    pub(super) measure: Duration,
}

impl Shape {
    pub(super) fn from_env() -> Self {
        let var = |name: &str, default: u64| -> u64 {
            std::env::var(name)
                .ok()
                .map_or(default, |text| text.parse().expect("a number"))
        };
        Self {
            key: var("PEPPYDB_BENCH_KEY", 8) as usize,
            value: var("PEPPYDB_BENCH_VALUE", 16) as usize,
            buffer_mib: var("PEPPYDB_BENCH_CIRCULAR_BUFFER_MIB", 32) as usize,
            keys: var("PEPPYDB_BENCH_KEYS", 1_000_000),
            measure: Duration::from_millis(var("PEPPYDB_BENCH_MILLIS", 1000)),
        }
    }

    pub(super) fn config(&self, record_cache: bool) -> TreeConfig {
        let defaults = TreeConfig::default();
        TreeConfig {
            max_key_len: defaults.max_key_len.max(self.key),
            max_value_len: defaults.max_value_len.max(self.value),
            circular_buffer_bytes: self.buffer_mib << 20,
            read_record_cache: record_cache,
            ..defaults
        }
    }

    /// The `i`th key: a scrambled index, padded to the key length.
    pub(super) fn key(&self, i: u64) -> Vec<u8> {
        let mut key = i.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes().to_vec();
        key.resize(self.key.max(8), 0x5a);
        key
    }

    pub(super) fn describe(&self) -> String {
        format!(
            "{} keys of {} bytes, {}-byte values, {} MiB circular buffer, {:?} per point",
            self.keys, self.key, self.value, self.buffer_mib, self.measure
        )
    }
}

/// A tree loaded with every key of the shape.
fn loaded(shape: &Shape, record_cache: bool) -> Tree {
    let tree = Tree::in_memory(shape.config(record_cache)).unwrap();
    let value = vec![7; shape.value];
    for i in 0..shape.keys {
        tree.put(&shape.key(i), &value).unwrap();
    }
    tree
}

/// A per-thread xorshift generator.
pub(super) struct Rng(pub(super) u64);

impl Rng {
    pub(super) fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

/// A mix of point operations: the percentage that are puts.
#[derive(Clone, Copy, Debug)]
pub(super) struct Mix {
    name: &'static str,
    put_percent: u64,
}

pub(super) const GET: Mix = Mix {
    name: "get",
    put_percent: 0,
};
pub(super) const PUT: Mix = Mix {
    name: "put",
    put_percent: 100,
};
const HALF: Mix = Mix {
    name: "50/50",
    put_percent: 50,
};
const MOSTLY_GET: Mix = Mix {
    name: "90/10",
    put_percent: 10,
};

/// Run `threads` threads of `mix` over `keys` (indices into the shape) for
/// the shape's measurement time; return the total rate in M ops/s.
pub(super) fn measure(tree: &Tree, shape: &Shape, keys: &[u64], threads: usize, mix: Mix) -> f64 {
    let stop = AtomicBool::new(false);
    let value = vec![9; shape.value];
    let started = Instant::now();
    let ops: u64 = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                let (stop, value) = (&stop, &value);
                scope.spawn(move || {
                    let mut rng = Rng(0x9e37_79b9 ^ ((t as u64 + 1) * 0x1234_5678_9abc));
                    let mut ops = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        for _ in 0..64 {
                            let key = shape.key(keys[rng.below(keys.len() as u64) as usize]);
                            if rng.below(100) < mix.put_percent {
                                tree.put(&key, value).unwrap();
                            } else {
                                let found = tree.get_with(&key, |value| value.is_some());
                                assert_eq!(found, Ok(true));
                            }
                        }
                        ops += 64;
                    }
                    ops
                })
            })
            .collect();
        std::thread::sleep(shape.measure);
        stop.store(true, Ordering::Relaxed);
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .sum()
    });
    ops as f64 / started.elapsed().as_secs_f64() / 1e6
}

/// 1, 2, 4, 8, and the available parallelism, in order and deduplicated.
pub(super) fn thread_counts() -> Vec<usize> {
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    let mut counts = vec![1, 2, 4, 8, available];
    counts.sort_unstable();
    counts.dedup();
    counts
}

fn mode_name(record_cache: bool) -> &'static str {
    if record_cache { "record" } else { "page" }
}

/// Uniform keys over the whole tree, by thread count, mix, and caching mode.
#[test]
#[ignore = "informational benchmark"]
fn baseline_threads() {
    let shape = Shape::from_env();
    println!("baseline_threads: {}", shape.describe());
    let keys: Vec<_> = (0..shape.keys).collect();
    for record_cache in [true, false] {
        let tree = loaded(&shape, record_cache);
        println!(
            "{} caching: height {}, {} leaves",
            mode_name(record_cache),
            tree.height(),
            tree.leaf_count()
        );
        for mix in [GET, PUT, HALF] {
            let rates: Vec<_> = thread_counts()
                .into_iter()
                .map(|threads| {
                    let rate = measure(&tree, &shape, &keys, threads, mix);
                    format!("{threads}t {rate:.2}")
                })
                .collect();
            println!("  {:>6}: {} M ops/s", mix.name, rates.join(", "));
        }
    }
}

/// The keys of the first `leaves` leaves in key order.
pub(super) fn hot_keys(tree: &Tree, shape: &Shape, leaves: usize) -> Vec<u64> {
    let mut by_key: Vec<_> = (0..shape.keys).map(|i| (shape.key(i), i)).collect();
    by_key.sort_unstable();
    let mut seen = Vec::new();
    by_key
        .into_iter()
        .take_while(|(key, _)| {
            let leaf = tree.find_leaf(key);
            if seen.last() != Some(&leaf) {
                seen.push(leaf);
            }
            seen.len() <= leaves
        })
        .map(|(_, i)| i)
        .collect()
}

/// Contention on a hot set: one leaf, and about 32 leaves.
#[test]
#[ignore = "informational benchmark"]
fn baseline_hot_set() {
    let shape = Shape::from_env();
    println!("baseline_hot_set: {}", shape.describe());
    let tree = loaded(&shape, true);
    for leaves in [1, 32] {
        let keys = hot_keys(&tree, &shape, leaves);
        println!("{leaves} leaves ({} keys):", keys.len());
        for mix in [GET, MOSTLY_GET] {
            let rates: Vec<_> = thread_counts()
                .into_iter()
                .map(|threads| {
                    let rate = measure(&tree, &shape, &keys, threads, mix);
                    format!("{threads}t {rate:.2}")
                })
                .collect();
            println!("  {:>6}: {} M ops/s", mix.name, rates.join(", "));
        }
    }
}

/// Time `f` over `count` iterations; return nanoseconds per iteration.
fn per_op(count: u64, mut f: impl FnMut(u64)) -> f64 {
    let started = Instant::now();
    for i in 0..count {
        f(i);
    }
    started.elapsed().as_secs_f64() * 1e9 / count as f64
}

/// Single-threaded costs of the pieces of a point operation.
#[test]
#[ignore = "informational benchmark"]
fn baseline_components() {
    let shape = Shape::from_env();
    println!("baseline_components: {}", shape.describe());
    let tree = loaded(&shape, true);
    println!("height {}, {} leaves", tree.height(), tree.leaf_count());
    const COUNT: u64 = 2_000_000;
    let keys: Vec<_> = (0..4096)
        .map(|i| shape.key(i * 7919 % shape.keys))
        .collect();
    let pick = |i: u64| &keys[(i % 4096) as usize];

    let descent = per_op(COUNT, |i| {
        std::hint::black_box(tree.route(pick(i)));
    });
    let leaf = tree.find_leaf(pick(0));
    let shared = per_op(COUNT, |_| {
        drop(std::hint::black_box(tree.leaves.read(leaf).unwrap()));
    });
    let exclusive = per_op(COUNT, |_| {
        drop(std::hint::black_box(tree.leaves.write(leaf).unwrap()));
    });
    let hot = pick(0).clone();
    let get_hot = per_op(COUNT, |_| {
        let found = tree.get_with(&hot, |value| value.is_some());
        assert_eq!(found, Ok(true));
    });
    let value = vec![3; shape.value];
    let put = per_op(COUNT / 4, |i| {
        tree.put(&shape.key(i.wrapping_mul(7919) % shape.keys), &value)
            .unwrap();
    });
    println!("descent (route):       {descent:.1} ns");
    println!("leaf S lock + unlock:  {shared:.1} ns");
    println!("leaf X lock + unlock:  {exclusive:.1} ns");
    println!("get_with, hot leaf:    {get_hot:.1} ns");
    println!("put, random leaves:    {put:.1} ns");
}
