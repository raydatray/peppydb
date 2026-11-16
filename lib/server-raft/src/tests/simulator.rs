//! The simulator itself: determinism, and that every fault occurs (plan
//! 0028, diff 10).

use crate::sim::Trace;
use crate::sim::simulator::Outcome;
use crate::sim::simulator::SimConfig;
use crate::sim::simulator::run;
use crate::sim::simulator::run_with;

#[test]
fn one_seed_gives_one_run() {
    for seed in [1, 2] {
        let a = Trace::keeping_lines();
        let b = Trace::keeping_lines();
        let first = run_with(seed, SimConfig::for_seed(seed), a.clone(), |_, _| {});
        let second = run_with(seed, SimConfig::for_seed(seed), b.clone(), |_, _| {});
        let (a, b) = (a.lines().unwrap(), b.lines().unwrap());
        if let Some(at) = a.iter().zip(&b).position(|(x, y)| x != y) {
            panic!(
                "seed {seed}: traces diverge at event {at}:\n  {}\n  {}",
                a[at], b[at]
            );
        }
        assert_eq!(first.trace_hash, second.trace_hash, "seed {seed}");
        assert_eq!(first.history, second.history, "seed {seed}");
        assert_eq!(first.contents, second.contents, "seed {seed}");
        assert_ne!(
            run(seed + 100).trace_hash,
            first.trace_hash,
            "another seed, another run"
        );
    }
}

#[test]
fn every_fault_occurs_and_the_cluster_recovers() {
    let reports: Vec<_> = (1..=6).map(run).collect();
    let total =
        |f: &dyn Fn(&crate::sim::simulator::Report) -> u64| reports.iter().map(f).sum::<u64>();
    assert!(total(&|r| r.net.dropped) > 0, "dropped messages");
    assert!(total(&|r| r.net.blocked) > 0, "partitioned links");
    assert!(total(&|r| r.net.lost_replies) > 0, "lost replies");
    assert!(total(&|r| r.net.duplicated) > 0, "duplicates");
    assert!(total(&|r| r.net.delayed) > 0, "delays");
    assert!(total(&|r| r.crashes) > 0, "crashes");
    assert!(total(&|r| r.partitions) > 0, "partitions");
    assert!(total(&|r| r.snapshots) > 0, "snapshots");
    assert!(total(&|r| r.transfers) > 0, "leader transfers");
    let acked = |r: &crate::sim::simulator::Report| {
        r.history
            .iter()
            .filter(|op| matches!(op.outcome, Outcome::Acked(_)))
            .count() as u64
    };
    let reads = |r: &crate::sim::simulator::Report| {
        r.history
            .iter()
            .filter(|op| matches!(op.outcome, Outcome::Read(_)))
            .count() as u64
    };
    assert!(
        total(&acked) > 0 && total(&reads) > 0,
        "the workload makes progress"
    );
    for report in &reports {
        let first = report.contents.values().next().unwrap();
        assert!(
            report.contents.values().all(|c| c == first),
            "seed {}: replicas converge",
            report.seed
        );
    }
}
