//! The simulator itself: determinism, and that every fault occurs (plan
//! 0028, diff 10); safety holds and the checker catches a broken log
//! (diff 11).

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
        report.assert_safe();
        let first = report.contents.values().next().unwrap();
        assert!(
            report.contents.values().all(|c| c == first),
            "seed {}: replicas converge",
            report.seed
        );
    }
}

/// The checker catches a log that loses unflushed votes, in every seed.
#[test]
fn a_log_that_loses_votes_is_caught() {
    for seed in 1..=10 {
        let mut config = SimConfig::for_seed(seed);
        config.lazy_votes = true;
        let report = run_with(seed, config, Trace::default(), |_, _| {});
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.starts_with("persist before send")),
            "seed {seed}: {:?}",
            report.violations
        );
    }
}

/// The simulator runs the real log by default (plan 0029); the `MemLog`
/// reference model still runs and stays safe.
#[test]
fn both_logs_run_safely() {
    use crate::sim::sim_log::LogKind;
    for seed in 1..=4 {
        for log in [LogKind::Disk, LogKind::Mem] {
            let mut config = SimConfig::for_seed(seed);
            assert_eq!(config.log, LogKind::Disk, "the real log is the default");
            config.log = log;
            let report = run_with(seed, config, Trace::default(), |cluster, _| {
                let kind = match cluster.log(1) {
                    crate::sim::sim_log::SimLog::Disk { .. } => LogKind::Disk,
                    crate::sim::sim_log::SimLog::Mem(_) => LogKind::Mem,
                };
                assert_eq!(kind, log);
            });
            report.assert_safe();
        }
    }
}

/// Injected file-system failures stop only the failing node; the cluster
/// stays safe and recovers once it restarts (plan 0029, fail-stop).
#[test]
fn injected_sync_failures_stop_only_the_failing_node() {
    let mut fail_stops = 0;
    for seed in 1..=12 {
        let mut config = SimConfig::for_seed(seed);
        config.sync_failures = true;
        let report = run_with(seed, config, Trace::default(), |_, _| {});
        report.assert_safe();
        let first = report.contents.values().next().unwrap();
        assert!(
            report.contents.values().all(|c| c == first),
            "seed {seed}: replicas converge"
        );
        fail_stops += report.fail_stops;
    }
    assert!(fail_stops > 0, "some node failed and stopped");
}

/// The simulator runs the real state machine by default (plan 0030); the
/// `ModelSm` reference model still runs and stays safe, and both see
/// snapshots and crashes.
#[test]
fn both_state_machines_run_safely() {
    use crate::sim::sim_sm::SimSm;
    use crate::sim::sim_sm::SmKind;
    let (mut snapshots, mut crashes) = (0, 0);
    for seed in 1..=4 {
        for sm in [SmKind::Db, SmKind::Model] {
            let mut config = SimConfig::for_seed(seed);
            assert_eq!(
                config.sm,
                SmKind::Db,
                "the real state machine is the default"
            );
            config.sm = sm;
            let report = run_with(seed, config, Trace::default(), |cluster, _| {
                let (_, running) = cluster.node(1).expect("node 1 is up");
                let kind = match running {
                    SimSm::Db(_) => SmKind::Db,
                    SimSm::Model(_) => SmKind::Model,
                };
                assert_eq!(kind, sm);
            });
            report.assert_safe();
            snapshots += report.snapshots;
            crashes += report.crashes;
        }
    }
    assert!(snapshots > 0 && crashes > 0);
}
