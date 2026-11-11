//! The seed tiers (plan 0028, "Two tiers of seeds").
//!
//! - **Default tier:** seeds `1..=DEFAULT_SEEDS` and every regression seed,
//!   on every test run, split into chunks that run in parallel.
//! - **One seed:** `PEPPY_SIM_SEED=<n>` reruns one seed with its trace.
//! - **Long tier:** an ignored test; `PEPPY_SIM_SEEDS` sets the count
//!   (default 100000) and `PEPPY_SIM_BASE` the first seed (default: drawn
//!   from the clock and printed).

use std::time::Instant;

use crate::sim::Trace;
use crate::sim::regressions::REGRESSIONS;
use crate::sim::simulator::Report;
use crate::sim::simulator::SimConfig;
use crate::sim::simulator::run;
use crate::sim::simulator::run_with;

/// The default tier's size, set from the measured cost per seed so that
/// the tier stays well under 60 s (plan 0028, "Verification").
const DEFAULT_SEEDS: u64 = 400;
const CHUNKS: u64 = 8;

fn sweep(chunk: u64) {
    let seeds = (1..=DEFAULT_SEEDS).filter(|seed| seed % CHUNKS == chunk);
    let mut sizes = [0, 0];
    for seed in seeds {
        let report = run(seed);
        report.assert_safe();
        assert!(report.elections > 0, "seed {seed}: no node ever led");
        sizes[usize::from(report.config.nodes == 5)] += 1;
    }
    assert!(
        sizes[0] > 0 && sizes[1] > 0,
        "both 3-node and 5-node clusters run: {sizes:?}"
    );
}

#[test]
fn default_tier_0() {
    sweep(0);
}
#[test]
fn default_tier_1() {
    sweep(1);
}
#[test]
fn default_tier_2() {
    sweep(2);
}
#[test]
fn default_tier_3() {
    sweep(3);
}
#[test]
fn default_tier_4() {
    sweep(4);
}
#[test]
fn default_tier_5() {
    sweep(5);
}
#[test]
fn default_tier_6() {
    sweep(6);
}
#[test]
fn default_tier_7() {
    sweep(7);
}

#[test]
fn regression_seeds() {
    for (seed, _) in REGRESSIONS {
        run(*seed).assert_safe();
    }
}

/// `PEPPY_SIM_SEED=<n>`: rerun one seed and print its summary and trace
/// hash. Without the variable, does nothing.
#[test]
fn one_seed() {
    let Ok(seed) = std::env::var("PEPPY_SIM_SEED") else {
        return;
    };
    let seed: u64 = seed.parse().expect("PEPPY_SIM_SEED is a u64");
    let trace = Trace::keeping_lines();
    let report = run_with(seed, SimConfig::for_seed(seed), trace.clone(), |_, _| {});
    if std::env::var_os("PEPPY_SIM_TRACE").is_some() {
        trace
            .lines()
            .unwrap_or_default()
            .iter()
            .for_each(|line| eprintln!("{line}"));
    }
    eprintln!("{}", summary(&report));
    report.assert_safe();
}

fn summary(r: &Report) -> String {
    format!(
        "seed {} hash {:016x}: {} nodes, {} ops, {} elections, {} crashes, {} partitions, {} snapshots, {} transfers, {:?}",
        r.seed,
        r.trace_hash,
        r.config.nodes,
        r.history.len(),
        r.elections,
        r.crashes,
        r.partitions,
        r.snapshots,
        r.transfers,
        r.net
    )
}

/// The long tier: many seeds from a printed base, across every core. Run
/// it in release mode: `tools/buck2 run @build/mode/release
/// //lib/server-raft:server-raft-unittest -- --ignored long_tier`.
#[test]
#[ignore]
fn long_tier() {
    let count: u64 =
        std::env::var("PEPPY_SIM_SEEDS").map_or(100_000, |n| n.parse().expect("a count"));
    let base: u64 = std::env::var("PEPPY_SIM_BASE").map_or_else(
        |_| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(1, |d| d.as_nanos() as u64);
            nanos % 1_000_000_000_000
        },
        |n| n.parse().expect("a seed"),
    );
    eprintln!("long tier: seeds {base}..{}", base + count);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get() as u64);
    let started = Instant::now();
    let failures: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                scope.spawn(move || {
                    (base..base + count)
                        .filter(|seed| seed % threads == t)
                        .filter_map(|seed| {
                            let report = run(seed);
                            (!report.violations.is_empty())
                                .then(|| format!("{}: {:?}", seed, report.violations))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("a worker panicked"))
            .collect()
    });
    let elapsed = started.elapsed();
    eprintln!(
        "long tier: {count} seeds in {elapsed:?} ({:?} per seed per thread)",
        elapsed * threads as u32 / count.max(1) as u32
    );
    assert!(
        failures.is_empty(),
        "failing seeds:\n{}",
        failures.join("\n")
    );
}
