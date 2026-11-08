//! Informational engine benchmarks, not gates (plan 0023). Run in release
//! mode, on Linux for contended and direct-I/O numbers (`tools/linux-run`
//! sets `PEPPYDB_BENCH_DIR` to an ext4 volume):
//!
//! ```text
//! tools/buck2 run @build/mode/release //crates/engine:engine-unittest -- --ignored --nocapture tests::bench::
//! ```
//!
//! `PEPPYDB_BENCH_MILLIS` sets the time per measurement (default 1000).
//! `Io::Direct` parts print "skipped" where `O_DIRECT` is refused.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::*;

fn bench_dir() -> tempfile::TempDir {
    match std::env::var_os("PEPPYDB_BENCH_DIR") {
        Some(base) => tempfile::tempdir_in(base).unwrap(),
        None => tempfile::tempdir().unwrap(),
    }
}

fn measure_time() -> Duration {
    let millis = std::env::var("PEPPYDB_BENCH_MILLIS")
        .ok()
        .map_or(1000, |text| text.parse().expect("a number"));
    Duration::from_millis(millis)
}

/// Default tree settings with a 64 MiB circular buffer; no checkpoint
/// during a measurement unless one is asked for.
fn options(io: Io, flush_interval: Duration) -> Options {
    Options {
        io,
        flush_interval,
        tree: TreeConfig {
            circular_buffer_bytes: 64 << 20,
            ..TreeConfig::default()
        },
        checkpoint_wal_bytes: u64::MAX,
        checkpoint_interval: Duration::from_secs(3600),
        ..Options::default()
    }
}

/// Open a database in `dir`, or `None` if `io` is refused.
fn open(dir: &tempfile::TempDir, options: Options) -> Option<Db> {
    match Db::open(dir.path(), options) {
        Ok(db) => Some(db),
        Err(Error::Tree(TreeError::Store(bf_tree::StoreError::DirectIo(reason)))) => {
            println!("  {:?}: skipped ({reason})", options.io);
            None
        }
        Err(Error::Tree(TreeError::Store(bf_tree::StoreError::Io {
            kind: std::io::ErrorKind::InvalidInput,
            ..
        }))) => {
            println!(
                "  {:?}: skipped (the file system refuses O_DIRECT)",
                options.io
            );
            None
        }
        Err(error) => panic!("{error:?}"),
    }
}

fn scramble(i: u64) -> [u8; 8] {
    i.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes()
}

/// Durable puts of 8-byte keys and 16-byte values on `threads` threads for
/// `time`; the rate and the sorted latencies in nanoseconds.
fn durable_puts(db: &Db, threads: usize, time: Duration) -> (f64, Vec<u64>) {
    let stop = AtomicBool::new(false);
    let started = Instant::now();
    let mut latencies: Vec<u64> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads as u64)
            .map(|t| {
                let stop = &stop;
                scope.spawn(move || {
                    let mut latencies = Vec::new();
                    let mut i = t << 40;
                    while !stop.load(Ordering::Relaxed) {
                        let started = Instant::now();
                        db.put(&scramble(i), &[7; 16]).unwrap();
                        latencies.push(started.elapsed().as_nanos() as u64);
                        i += 1;
                    }
                    latencies
                })
            })
            .collect();
        std::thread::sleep(time);
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

fn percentiles(sorted: &[u64]) -> String {
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize] as f64 / 1e3;
    format!(
        "p50 {:.0} µs, p99 {:.0} µs, max {:.0} µs",
        at(0.5),
        at(0.99),
        at(1.0)
    )
}

fn thread_counts() -> Vec<usize> {
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    let mut counts = vec![1, 2, 4, 8, available];
    counts.sort_unstable();
    counts.dedup();
    counts
}

/// Durable put rate, latency, and average group size by thread count,
/// flush interval (0 and 1 ms), and store I/O mode. Then the WAL fsync
/// interference test: one thread's put latency alone, and while a loader
/// thread writes batches of 1000 random puts and another thread
/// checkpoints in a loop, so the store writes through while the WAL syncs.
#[test]
#[ignore = "informational benchmark"]
fn bench_engine_write() {
    let time = measure_time();
    println!("bench_engine_write: {time:?} per point");
    for io in [Io::Cached, Io::Direct] {
        for flush_interval in [Duration::ZERO, Duration::from_millis(1)] {
            let dir = bench_dir();
            let Some(db) = open(&dir, options(io, flush_interval)) else {
                break;
            };
            println!("  {io:?}, flush_interval {flush_interval:?}:");
            for threads in thread_counts() {
                let wal = db.shared.wal.as_ref().expect("standalone");
                let syncs = wal.syncs();
                let (rate, latencies) = durable_puts(&db, threads, time);
                let groups = wal.syncs() - syncs;
                println!(
                    "    {threads:>2}t: {:>8.0} puts/s, {}, {:.1} puts per group",
                    rate,
                    percentiles(&latencies),
                    latencies.len() as f64 / groups.max(1) as f64,
                );
            }
            db.close().unwrap();
        }
    }
    println!("  fsync interference, 1 measuring thread, flush_interval 0:");
    for io in [Io::Cached, Io::Direct] {
        let dir = bench_dir();
        let Some(db) = open(&dir, options(io, Duration::ZERO)) else {
            continue;
        };
        let (_, alone) = durable_puts(&db, 1, time);
        let stop = AtomicBool::new(false);
        let (checkpoints, loaded) = (AtomicU64::new(0), AtomicU64::new(0));
        let (_, loaded_latencies) = std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut i = 1u64 << 50;
                while !stop.load(Ordering::Relaxed) {
                    let mut batch = WriteBatch::new();
                    for _ in 0..1000 {
                        batch.put(&scramble(i), &[9; 16]);
                        i += 1;
                    }
                    db.write(batch).unwrap();
                    loaded.fetch_add(1000, Ordering::Relaxed);
                }
            });
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    db.checkpoint().unwrap();
                    checkpoints.fetch_add(1, Ordering::Relaxed);
                }
            });
            let result = durable_puts(&db, 1, time);
            stop.store(true, Ordering::Relaxed);
            result
        });
        println!(
            "    {io:?}: alone {}; with load and checkpoints {} \
             ({} loaded puts, {} checkpoints)",
            percentiles(&alone),
            percentiles(&loaded_latencies),
            loaded.into_inner(),
            checkpoints.into_inner(),
        );
        db.close().unwrap();
    }
}

/// WAL replay rate at open, onto a store in each I/O mode: write records
/// with no checkpoint, drop the database (no checkpoint; the WAL drains),
/// and time the reopen.
#[test]
#[ignore = "informational benchmark"]
fn bench_wal_recovery() {
    const RECORDS: u64 = 200_000;
    const THREADS: u64 = 32;
    println!("bench_wal_recovery: {RECORDS} single-put records");
    for io in [Io::Cached, Io::Direct] {
        let dir = bench_dir();
        let Some(db) = open(&dir, options(io, Duration::ZERO)) else {
            continue;
        };
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let db = &db;
                scope.spawn(move || {
                    (0..RECORDS / THREADS)
                        .for_each(|i| db.put(&scramble(t << 40 | i), &[7; 16]).unwrap());
                });
            }
        });
        let applied = db.applied_seq();
        drop(db);
        let started = Instant::now();
        let db = Db::open(dir.path(), options(io, Duration::ZERO)).unwrap();
        let elapsed = started.elapsed().as_secs_f64();
        assert_eq!(db.applied_seq(), applied);
        println!(
            "  {io:?}: open with replay {:.0} ms, {:.0} records/s",
            elapsed * 1e3,
            RECORDS as f64 / elapsed,
        );
        db.close().unwrap();
    }
}
