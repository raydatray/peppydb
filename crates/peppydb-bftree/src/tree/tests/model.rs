use std::collections::BTreeMap;
use std::ops::Bound;
use std::time::Instant;

use super::*;
use crate::UpsertOutcome;

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

/// Run `steps` random operations against the tree and a `BTreeMap`, checking
/// results and invariants after every step.
fn run(config: TreeConfig, keys: Keys, seed: u64, steps: usize, min_height: usize) {
    let mut rng = Rng(seed);
    let mut tree = Tree::in_memory(config).unwrap();
    let mut model = Model::new();
    let max_key = config.max_key_len;
    let mut max_height = 1;
    for step in 0..steps {
        let key = generate_key(&mut rng, keys, max_key);
        let context = format!("{config:?} {keys:?} seed {seed:#x} step {step}");
        match rng.below(10) {
            0..=4 => {
                let value_len = match rng.below(4) {
                    0 => config.max_value_len,
                    _ => rng.below(config.max_value_len + 1),
                };
                let value = vec![step as u8; value_len];
                let expected = match model.insert(key.clone(), value.clone()) {
                    Some(_) => UpsertOutcome::Replaced,
                    None => UpsertOutcome::Inserted,
                };
                assert_eq!(tree.put(&key, &value), Ok(expected), "{context}");
            }
            5..=6 => {
                let expected = model.remove(&key).is_some();
                assert_eq!(tree.remove(&key), Ok(expected), "{context}");
            }
            7 => {
                let expected = model.get(&key).map(Vec::as_slice);
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
        assert_eq!(tree.check_invariants(), Ok(()), "{context}");
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
fn reversed_ranges_are_rejected() {
    let tree = Tree::in_memory(TreeConfig::default()).unwrap();
    assert_eq!(
        tree.range(Some(b"b"), Some(b"a")).err(),
        Some(TreeError::InvalidRange)
    );
    assert_eq!(tree.range(Some(b"a"), Some(b"a")).unwrap().count(), 0);
}

/// An informational single-threaded baseline, not a gate. Run with
/// `cargo test --release -p peppydb-bftree -- --ignored --nocapture baseline`.
#[test]
#[ignore = "informational benchmark"]
fn baseline_throughput() {
    const COUNT: u64 = 1_000_000;
    let mut tree = Tree::in_memory(TreeConfig::default()).unwrap();
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
