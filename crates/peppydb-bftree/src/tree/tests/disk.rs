//! An informational larger-than-memory `FileStore` benchmark.
//!
//! This stack never evicts, so a tree cannot be built through `put` on a
//! dataset larger than RAM. The benchmark instead writes the store file
//! directly in the on-disk format (sorted keys, three ~1 KiB records per
//! 4 KiB leaf), opens it with `Tree::open`, and then measures random point
//! lookups, range scans, random replacements, and `flush`. Latency
//! percentiles separate page-cache hits from disk reads, because the page
//! cache cannot be dropped without root on macOS.
//!
//! ```sh
//! PEPPYDB_DISK_GIB=96 cargo test --release -p peppydb-bftree -- --ignored --nocapture disk_baseline
//! ```
//!
//! `PEPPYDB_DISK_DIR` chooses where the file lives (default: a temporary
//! directory, removed afterwards).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::*;
use crate::{FileStore, UpsertOutcome};
use zerocopy::IntoBytes;

const VALUE_LEN: usize = 1000;
const PER_LEAF: u64 = 3;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

struct Latencies(Vec<u64>);

impl Latencies {
    fn time<T>(&mut self, op: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let result = op();
        self.0.push(started.elapsed().as_nanos() as u64);
        result
    }

    fn report(&mut self, name: &str, unit: &str) {
        self.0.sort_unstable();
        let total: u64 = self.0.iter().sum();
        let at = |q: f64| {
            let index = ((self.0.len() - 1) as f64 * q) as usize;
            Duration::from_nanos(self.0[index])
        };
        println!(
            "{name:<22} {:>9.0} {unit}/s  p50 {:>9.1?}  p90 {:>9.1?}  p99 {:>9.1?}  p99.9 {:>9.1?}  max {:>9.1?}",
            self.0.len() as f64 / Duration::from_nanos(total).as_secs_f64(),
            at(0.5),
            at(0.9),
            at(0.99),
            at(0.999),
            at(1.0),
        );
    }
}

/// Write `leaves` sorted leaves straight into a store file.
fn write_store(path: &PathBuf, config: TreeConfig, leaves: u64) -> u64 {
    let superblock = Superblock::from_config(&config);
    let mut file = BufWriter::with_capacity(16 << 20, File::create(path).unwrap());
    let mut page = vec![0; config.leaf_capacity];
    superblock.encode(&mut page).unwrap();
    file.write_all(&page).unwrap();

    let value = vec![0xab; VALUE_LEN];
    for leaf in 0..leaves {
        let first = leaf * PER_LEAF;
        let lower = first.to_be_bytes();
        let upper = (first + PER_LEAF).to_be_bytes();
        let fences = Fences {
            lower: (leaf > 0)
                .then_some(lower.as_slice())
                .map_or(Fence::Unbounded, Fence::Key),
            upper: (leaf + 1 < leaves)
                .then_some(upper.as_slice())
                .map_or(Fence::Unbounded, Fence::Key),
        };
        let page = Page::init_with_fences(&mut page, fences).unwrap();
        for key in first..first + PER_LEAF {
            page.try_insert(&key.to_be_bytes(), &value).unwrap();
        }
        file.write_all(page.as_bytes()).unwrap();
    }
    let file = file.into_inner().unwrap();
    file.sync_all().unwrap();
    file.metadata().unwrap().len()
}

#[test]
#[ignore = "informational benchmark; writes a file larger than memory"]
fn disk_baseline() {
    let gib: u64 = std::env::var("PEPPYDB_DISK_GIB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(96);
    let config = TreeConfig::default();
    let leaves = (gib << 30) / config.leaf_capacity as u64;
    let records = leaves * PER_LEAF;
    let tempdir = tempfile::tempdir().unwrap();
    let dir = std::env::var_os("PEPPYDB_DISK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| tempdir.path().to_path_buf());
    let path = dir.join("disk-baseline.bf");
    let _ = std::fs::remove_file(&path);
    println!(
        "{gib} GiB target: {leaves} leaves, {records} records of {VALUE_LEN}-byte values at {}",
        path.display()
    );

    let started = Instant::now();
    let bytes = write_store(&path, config, leaves);
    let elapsed = started.elapsed();
    println!(
        "bulk write:            {elapsed:.1?}  {:.2} GiB/s",
        bytes as f64 / (1u64 << 30) as f64 / elapsed.as_secs_f64()
    );

    let started = Instant::now();
    let store = FileStore::open(&path).unwrap();
    let elapsed = started.elapsed();
    println!("FileStore::open:       {elapsed:.1?}");

    let started = Instant::now();
    let mut tree = Tree::open(Box::new(store)).unwrap();
    let elapsed = started.elapsed();
    println!(
        "Tree::open:            {elapsed:.1?}  {:.2} GiB/s read+validate, height {}, {} inner nodes",
        bytes as f64 / (1u64 << 30) as f64 / elapsed.as_secs_f64(),
        tree.height(),
        tree.inner.len()
    );

    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let gets = 200_000;
    let mut latencies = Latencies(Vec::with_capacity(gets));
    for _ in 0..gets {
        let key = (rng.next() % records).to_be_bytes();
        let found = latencies.time(|| tree.get(&key).unwrap().is_some());
        assert!(found);
    }
    latencies.report("get, first touch", "ops");

    let mut latencies = Latencies(Vec::with_capacity(gets));
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..gets {
        let key = (rng.next() % records).to_be_bytes();
        latencies.time(|| tree.get(&key).unwrap());
    }
    latencies.report("get, same keys warm", "ops");

    let scans = 5_000;
    let span = 100;
    let mut latencies = Latencies(Vec::with_capacity(scans));
    let mut scanned = 0;
    for _ in 0..scans {
        let start = rng.next() % (records - span);
        let (start, end) = (start.to_be_bytes(), (start + span).to_be_bytes());
        scanned += latencies.time(|| {
            tree.range(Some(&start), Some(&end))
                .unwrap()
                .filter(|record| record.is_ok())
                .count()
        });
    }
    assert_eq!(scanned as u64, scans as u64 * span);
    latencies.report(&format!("scan {span} records"), "scans");

    let puts = 100_000;
    let value = vec![0xcd; VALUE_LEN / 2];
    let mut latencies = Latencies(Vec::with_capacity(puts));
    for _ in 0..puts {
        let key = (rng.next() % records).to_be_bytes();
        let outcome = latencies.time(|| tree.put(&key, &value).unwrap());
        assert_eq!(outcome, UpsertOutcome::Replaced);
    }
    latencies.report("put, replace random", "ops");

    let dirty = tree.dirty_leaves();
    let started = Instant::now();
    tree.flush().unwrap();
    let elapsed = started.elapsed();
    println!(
        "flush:                 {elapsed:.1?}  {dirty} dirty leaves, {:.0} leaves/s, {:.1} MiB/s",
        dirty as f64 / elapsed.as_secs_f64(),
        (dirty * config.leaf_capacity) as f64 / (1 << 20) as f64 / elapsed.as_secs_f64()
    );
    println!(
        "resident after run:    {} leaves ({:.1} GiB)",
        tree.resident_leaves(),
        (tree.resident_leaves() * config.leaf_capacity) as f64 / (1u64 << 30) as f64
    );

    drop(tree);
    let _ = std::fs::remove_file(&path);
}
