//! Informational WAL append benchmark, not a gate (plan 0023). Run in
//! release mode, on Linux for contended numbers:
//!
//! ```text
//! tools/buck2 run @build/mode/release //lib/engine-wal:engine-wal-unittest -- --ignored --nocapture tests::bench::
//! ```
//!
//! Segments go in `PEPPYDB_BENCH_DIR`, or in the system temporary directory.

use std::time::Instant;

use crate::{Op, Wal, WalOptions};

/// The cost of `append`, which holds the pending mutex while it encodes and
/// copies a record. One thread measures the uncontended cost, which bounds
/// the time inside the mutex; all threads measure how appends serialise on
/// it. The flusher writes and syncs concurrently. Values of 16 bytes and
/// 1 KiB, with 8-byte keys.
#[test]
#[ignore = "informational benchmark"]
fn bench_wal_append() {
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    for (value_len, count) in [(16, 2_000_000u64), (1024, 200_000)] {
        let value = vec![7; value_len];
        for threads in [1, threads] {
            let dir = match std::env::var_os("PEPPYDB_BENCH_DIR") {
                Some(base) => tempfile::tempdir_in(base).unwrap(),
                None => tempfile::tempdir().unwrap(),
            };
            let wal = Wal::open(dir.path(), WalOptions::default(), 1).unwrap();
            let per_thread = count / threads as u64;
            let started = Instant::now();
            std::thread::scope(|scope| {
                for t in 0..threads as u64 {
                    let (wal, value) = (&wal, &value);
                    scope.spawn(move || {
                        for i in 0..per_thread {
                            let key = (t * per_thread + i).to_be_bytes();
                            wal.append(Op::Put { key: &key, value }).unwrap();
                        }
                    });
                }
            });
            let elapsed = started.elapsed();
            let appends = per_thread * threads as u64;
            println!(
                "bench_wal_append: {value_len:>4}-byte values, {threads:>2}t: \
                 {:.0} ns per append per thread, {:.2} M appends/s in total",
                elapsed.as_nanos() as f64 * threads as f64 / appends as f64,
                appends as f64 / elapsed.as_secs_f64() / 1e6,
            );
            wal.close().unwrap();
        }
    }
}
