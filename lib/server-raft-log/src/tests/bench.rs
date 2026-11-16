//! Informational benchmarks of the log alone, not gates (plan 0029,
//! "Benchmarks"). Run in release mode:
//!
//! ```text
//! tools/buck2 run @build/mode/release //lib/server-raft-log:server-raft-log-unittest -- --ignored --nocapture tests::bench::
//! ```
//!
//! `PEPPYDB_BENCH_DIR` sets the directory (default: the system temporary
//! directory) and `PEPPYDB_BENCH_MILLIS` the time per measurement (default
//! 1000).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{EntryKind, LogEntry, LogOptions, RaftLog, RealFs};

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

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p) as usize]
}

/// One appender keeps up to `depth` 100-byte entries waiting for
/// durability; a flusher thread flushes whenever writes are queued, and
/// records each write's latency from append to durable.
fn measure(depth: u64) {
    let dir = bench_dir();
    let (log, _) = RaftLog::open(RealFs, dir.path(), LogOptions::default()).unwrap();
    let log = Arc::new(log);
    let submitted: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let flusher = {
        let (log, submitted, stop) = (Arc::clone(&log), Arc::clone(&submitted), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut latencies = Vec::new();
            let mut seen = 0;
            while !stop.load(Ordering::Relaxed) || log.durable() < log.submitted() {
                if log.durable() == log.submitted() {
                    std::thread::yield_now();
                    continue;
                }
                let durable = log.flush().unwrap();
                let now = Instant::now();
                let times = submitted.lock().unwrap();
                latencies.extend((seen..durable).map(|id| now - times[id as usize]));
                seen = durable;
            }
            latencies
        })
    };
    let started = Instant::now();
    let deadline = started + measure_time();
    let mut index = 1;
    while Instant::now() < deadline {
        if log.submitted() - log.durable() >= depth {
            std::thread::yield_now();
            continue;
        }
        let entry = LogEntry {
            index,
            term: 1,
            kind: EntryKind::Normal,
            payload: vec![7; 100],
        };
        let mut times = submitted.lock().unwrap();
        log.append(vec![entry]).unwrap();
        times.push(Instant::now());
        index += 1;
    }
    stop.store(true, Ordering::Relaxed);
    let mut latencies = flusher.join().unwrap();
    let elapsed = started.elapsed();
    latencies.sort_unstable();
    let entries = latencies.len() as f64;
    println!(
        "  depth {depth:>3}: {:>8.0} entries/s, p50 {:>8.1?}, p99 {:>8.1?}, p99.9 {:>8.1?}, {:>5.1} entries per sync",
        entries / elapsed.as_secs_f64(),
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.99),
        percentile(&latencies, 0.999),
        entries / log.flushes() as f64,
    );
}

#[test]
#[ignore = "informational benchmark"]
fn bench_log_appends() {
    println!("appends of 100-byte entries, one appender and one flusher:");
    for depth in [1, 4, 16, 64, 256] {
        measure(depth);
    }
}
