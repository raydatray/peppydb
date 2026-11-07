use std::collections::BTreeMap;
use std::ops::Bound;
use std::time::Instant;

use super::*;
use bf_tree_store::FileStore;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

/// How a run generates keys.
#[derive(Clone, Copy, Debug)]
enum Keys {
    /// Any byte, any length up to the limit.
    Uniform,
    /// A long shared prefix and a short tail, stressing prefix compression and
    /// per-side prefix sizing in leaf splits.
    SharedPrefix,
    /// Three byte values, including zero and 0xff, with keys that are
    /// prefixes of each other.
    Narrow,
    /// Every key at the maximum length.
    Maximum,
}

fn generate_key(rng: &mut Rng, keys: Keys, max_len: usize) -> Vec<u8> {
    match keys {
        Keys::Uniform => (0..1 + rng.below(max_len))
            .map(|_| rng.below(256) as u8)
            .collect(),
        Keys::SharedPrefix => {
            let prefix = b"tenant/0042/orders/2026-10-22/";
            let shared = (max_len / 2).min(prefix.len());
            let tail_len = 1 + rng.below((max_len - shared).min(6));
            let tail = (0..tail_len).map(|_| b"az\0\xff"[rng.below(4)]);
            prefix[..shared].iter().copied().chain(tail).collect()
        }
        Keys::Narrow => (0..1 + rng.below(max_len))
            .map(|_| [0, b'a', 0xff][rng.below(3)])
            .collect(),
        Keys::Maximum => (0..max_len).map(|_| rng.below(4) as u8).collect(),
    }
}

/// Where a run keeps its leaves, and whether it flushes and reopens.
#[derive(Clone, Copy, Debug)]
enum Backend {
    /// `Tree::in_memory`: a `MemStore`, never reopened.
    Memory,
    /// A `MemStore` flushed and reopened every `reopen_every` steps.
    MemoryReopen,
    /// A `FileStore` in a temporary directory, flushed, dropped, and
    /// reopened from its path every `reopen_every` steps.
    FileReopen,
}

/// Run `steps` random operations against the tree and a `BTreeMap`, checking
/// results and invariants after every step.
fn run(config: TreeConfig, keys: Keys, seed: u64, steps: usize, min_height: usize) {
    run_on(Backend::Memory, config, keys, seed, steps, min_height);
}

fn run_on(
    backend: Backend,
    config: TreeConfig,
    keys: Keys,
    seed: u64,
    steps: usize,
    min_height: usize,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model.bf");
    let superblock = Superblock::from(&config);
    let mut tree = match backend {
        Backend::Memory => Tree::in_memory(config).unwrap(),
        Backend::MemoryReopen => {
            Tree::create(config, Box::new(MemStore::new(superblock).unwrap())).unwrap()
        }
        Backend::FileReopen => Tree::create(
            config,
            Box::new(FileStore::create(&path, superblock).unwrap()),
        )
        .unwrap(),
    };
    let reopen_every = steps / 6 + 1;

    let mut rng = Rng(seed);
    let mut model = Model::new();
    let max_key = config.max_key_len;
    let mut max_height = 1;
    // With no promotion and no second chance, reads and scans never
    // allocate from the circular buffer.
    let read_only_reads = config.read_promotion_percent == 0
        && config.scan_promotion_percent == 0
        && config.second_chance_percent == 0;
    for step in 0..steps {
        let key = generate_key(&mut rng, keys, max_key);
        let context = format!("{backend:?} {config:?} {keys:?} seed {seed:#x} step {step}");
        if step % reopen_every == reopen_every - 1 {
            tree.checkpoint().unwrap();
            tree = match backend {
                Backend::Memory => tree,
                // Reopen with the same runtime settings, such as the budget.
                Backend::MemoryReopen => Tree::open_with(config, tree.into_store()).unwrap(),
                Backend::FileReopen => {
                    drop(tree);
                    Tree::open_with(config, Box::new(FileStore::open(&path).unwrap())).unwrap()
                }
            };
            assert_eq!(tree.check_invariants(), Ok(()), "{context}");
        }
        let used = tree.circular_buffer_used();
        let op = rng.below(10);
        match op {
            0..=4 => {
                let value_len = match rng.below(4) {
                    0 => config.max_value_len,
                    _ => rng.below(config.max_value_len + 1),
                };
                let value = vec![step as u8; value_len];
                model.insert(key.clone(), value.clone());
                assert_eq!(tree.put(&key, &value), Ok(()), "{context}");
            }
            5..=6 => {
                model.remove(&key);
                assert_eq!(tree.remove(&key), Ok(()), "{context}");
                assert_eq!(tree.get(&key), Ok(None), "{context}");
            }
            7 => {
                let expected = model.get(&key).cloned();
                assert_eq!(tree.get(&key), Ok(expected), "{context}");
            }
            _ => {
                let other = generate_key(&mut rng, keys, max_key);
                let (start, end) = match rng.below(4) {
                    0 => (None, Some(key)),
                    1 => (Some(key), None),
                    _ if key <= other => (Some(key), Some(other)),
                    _ => (Some(other), Some(key)),
                };
                let actual = collect(tree.range(start.as_deref(), end.as_deref()).unwrap());
                let bounds = (
                    start.as_deref().map_or(Bound::Unbounded, Bound::Included),
                    end.as_deref().map_or(Bound::Unbounded, Bound::Excluded),
                );
                let expected: Vec<_> = model
                    .range::<[u8], _>(bounds)
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                assert_eq!(actual, expected, "{context}");
            }
        }
        if read_only_reads && op >= 7 {
            assert_eq!(tree.circular_buffer_used(), used, "{context}");
        }
        assert_eq!(tree.check_invariants(), Ok(()), "{context}");
        assert!(
            tree.circular_buffer_used() <= config.circular_buffer_bytes,
            "{context}: circular buffer over budget"
        );
        max_height = max_height.max(tree.height());
    }
    // Every run must exercise splits; small nodes also split inner nodes.
    assert!(
        max_height >= min_height,
        "{config:?} {keys:?} reached height {max_height}"
    );

    // Final full comparison.
    let all = collect(tree.iter());
    let expected: Vec<_> = model.into_iter().collect();
    assert_eq!(all, expected);
}

#[test]
fn small_nodes_match_the_oracle() {
    let configs = [config(256, 128, 8, 16), config(128, 64, 8, 0)];
    for config in configs {
        for keys in [Keys::Uniform, Keys::Narrow, Keys::Maximum] {
            for seed in [1, 0xdead_beef] {
                run(config, keys, seed, 700, 3);
            }
        }
    }
}

#[test]
fn medium_nodes_with_shared_prefixes_match_the_oracle() {
    let configs = [config(512, 256, 24, 64), config(1024, 192, 32, 200)];
    for config in configs {
        for keys in [Keys::SharedPrefix, Keys::Uniform, Keys::Maximum] {
            for seed in [7, 0x0bad_cafe] {
                run(config, keys, seed, 700, 3);
            }
        }
    }
}

#[test]
fn default_nodes_match_the_oracle() {
    let config = TreeConfig::default();
    for keys in [Keys::SharedPrefix, Keys::Maximum, Keys::Uniform] {
        run(config, keys, 0x1234_5678, 1200, 2);
    }
}

#[test]
fn flush_and_reopen_on_memory_matches_the_oracle() {
    let configs = [config(256, 128, 8, 16), config(1024, 192, 32, 200)];
    for config in configs {
        for keys in [Keys::Uniform, Keys::SharedPrefix, Keys::Narrow] {
            run_on(Backend::MemoryReopen, config, keys, 0x0005_eed1, 700, 3);
        }
    }
}

#[test]
fn flush_and_reopen_on_file_matches_the_oracle() {
    let configs = [config(256, 128, 8, 16), config(512, 256, 24, 64)];
    for config in configs {
        for keys in [Keys::Uniform, Keys::Maximum] {
            run_on(Backend::FileReopen, config, keys, 0xf11e, 700, 3);
        }
    }
    run_on(
        Backend::FileReopen,
        TreeConfig::default(),
        Keys::SharedPrefix,
        0xf11e,
        1200,
        2,
    );
}

/// Circular buffers of about four and eight leaf pages force constant
/// eviction; second-chance shares of 0, 10, and 100 percent cover no
/// relocation, the default, and relocation on every access.
#[test]
fn tiny_circular_buffers_match_the_oracle_through_eviction() {
    for (bytes, full) in [(1024, false), (2048, true), (1024, true)] {
        for percent in [0, 10, 100] {
            let config = TreeConfig {
                circular_buffer_bytes: bytes,
                second_chance_percent: percent,
                write_load_full_page: full,
                ..config(256, 128, 8, 16)
            };
            run(config, Keys::Uniform, 0xe71c + u64::from(percent), 900, 3);
            run_on(Backend::MemoryReopen, config, Keys::Narrow, 0x5eed, 600, 2);
            run_on(Backend::FileReopen, config, Keys::Maximum, 0xf11e, 400, 2);
        }
    }
}

/// Every caching policy against the oracle: read rates of 0, 20, and 100
/// percent in record and page mode, scan rates of 0 and 100, write-load
/// full pages off and on, and circular buffers of about four, eight, and
/// sixteen leaf pages. Backends rotate so every policy also meets reopens.
#[test]
fn every_caching_policy_matches_the_oracle() {
    let mut runs = 0u64;
    for read in [0u8, 20, 100] {
        for record in [true, false] {
            if read == 0 && !record {
                continue; // The mode is irrelevant at rate 0.
            }
            for scan in [0u8, 100] {
                for full in [false, true] {
                    for bytes in [1024, 2048, 4096] {
                        let config = TreeConfig {
                            circular_buffer_bytes: bytes,
                            read_promotion_percent: read,
                            read_record_cache: record,
                            scan_promotion_percent: scan,
                            write_load_full_page: full,
                            second_chance_percent: if runs.is_multiple_of(2) { 0 } else { 10 },
                            policy_seed: runs,
                            ..config(256, 128, 8, 16)
                        };
                        let (backend, keys) = match runs % 3 {
                            0 => (Backend::Memory, Keys::Uniform),
                            1 => (Backend::MemoryReopen, Keys::Narrow),
                            _ => (Backend::FileReopen, Keys::Maximum),
                        };
                        run_on(backend, config, keys, 0xcac4e + runs, 500, 2);
                        runs += 1;
                    }
                }
            }
        }
    }
    assert_eq!(runs, 60);
}

#[test]
fn reversed_ranges_are_rejected() {
    let tree = Tree::in_memory(TreeConfig::default()).unwrap();
    assert_eq!(
        tree.range(Some(b"b"), Some(b"a")).err(),
        Some(TreeError::InvalidRange)
    );
    assert_eq!(tree.range(Some(b"a"), Some(b"a")).unwrap().count(), 0);
}

/// The default configuration: eviction keeps a million keys within the
/// default 32 MiB circular buffer.
fn baseline_config() -> TreeConfig {
    TreeConfig::default()
}

/// An informational single-threaded baseline, not a gate. Run with
/// `tools/buck2 run @build/mode/release //crates/bf-tree:bf-tree-unittest -- --ignored --nocapture baseline`.
#[test]
#[ignore = "informational benchmark"]
fn baseline_throughput() {
    const COUNT: u64 = 1_000_000;
    let tree = Tree::in_memory(baseline_config()).unwrap();
    let key = |i: u64| i.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes();
    let value = [7; 16];

    let started = Instant::now();
    for i in 0..COUNT {
        tree.put(&key(i), &value).unwrap();
    }
    let put = started.elapsed();

    let started = Instant::now();
    let found = (0..COUNT)
        .filter(|&i| tree.get(&key(i)).unwrap().is_some())
        .count();
    let get = started.elapsed();
    assert_eq!(found as u64, COUNT);

    let started = Instant::now();
    let scanned = tree.iter().count();
    let scan = started.elapsed();
    assert_eq!(scanned as u64, COUNT);

    let rate = |elapsed: std::time::Duration| COUNT as f64 / elapsed.as_secs_f64() / 1e6;
    println!(
        "{COUNT} random 8-byte keys, 16-byte values, height {}, {} leaves, {} inner nodes",
        tree.height(),
        tree.leaf_count(),
        tree.inner_count()
    );
    println!("put:  {:.2} M ops/s", rate(put));
    println!("get:  {:.2} M ops/s", rate(get));
    println!("scan: {:.2} M records/s", rate(scan));
}

/// An informational `FileStore` baseline: flush, open, and cold versus warm
/// point lookups. Run with
/// `tools/buck2 run @build/mode/release //crates/bf-tree:bf-tree-unittest -- --ignored --nocapture baseline_file`.
#[test]
#[ignore = "informational benchmark"]
fn baseline_file_store() {
    const COUNT: u64 = 1_000_000;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("baseline.bf");
    let config = baseline_config();
    let key = |i: u64| i.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes();
    let value = [7; 16];
    let rate = |elapsed: std::time::Duration| COUNT as f64 / elapsed.as_secs_f64() / 1e6;

    let store = FileStore::create(&path, Superblock::from(&config)).unwrap();
    let tree = Tree::create(config, Box::new(store)).unwrap();
    let started = Instant::now();
    for i in 0..COUNT {
        tree.put(&key(i), &value).unwrap();
    }
    let put = started.elapsed();
    let leaves = tree.leaf_count();

    let started = Instant::now();
    tree.checkpoint().unwrap();
    let flush = started.elapsed();
    drop(tree);
    let bytes = std::fs::metadata(&path).unwrap().len();

    let started = Instant::now();
    let tree = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    let open = started.elapsed();

    let started = Instant::now();
    let found = (0..COUNT)
        .filter(|&i| tree.get(&key(i)).unwrap().is_some())
        .count();
    let cold = started.elapsed();
    assert_eq!(found as u64, COUNT);

    let started = Instant::now();
    let found = (0..COUNT)
        .filter(|&i| tree.get(&key(i)).unwrap().is_some())
        .count();
    let warm = started.elapsed();
    assert_eq!(found as u64, COUNT);

    let started = Instant::now();
    let scanned = tree.iter().count();
    let scan = started.elapsed();
    assert_eq!(scanned as u64, COUNT);

    println!(
        "{COUNT} random 8-byte keys, 16-byte values, height {}, {leaves} leaves, {:.1} MiB file",
        tree.height(),
        bytes as f64 / (1024.0 * 1024.0)
    );
    println!("put (memory):  {:.2} M ops/s", rate(put));
    println!(
        "flush:         {flush:?} ({:.0} leaves/s)",
        leaves as f64 / flush.as_secs_f64()
    );
    println!(
        "open:          {open:?} ({:.0} leaves/s)",
        leaves as f64 / open.as_secs_f64()
    );
    println!(
        "get cold:      {:.2} M ops/s (~{:.1} us per leaf load over warm)",
        rate(cold),
        cold.saturating_sub(warm).as_secs_f64() * 1e6 / leaves as f64
    );
    println!("get warm:      {:.2} M ops/s", rate(warm));
    println!("scan warm:     {:.2} M records/s", rate(scan));
}
