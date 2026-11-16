//! Informational benchmark of three nodes over the real log on real files,
//! not a gate (plan 0029, "Benchmarks"). The nodes run in one process on
//! Tokio over a perfect in-memory network, so the log's syncs dominate.
//! Run in release mode:
//!
//! ```text
//! tools/buck2 run @build/mode/release //lib/server-raft:server-raft-unittest -- --ignored --nocapture tests::bench::
//! ```
//!
//! `PEPPYDB_BENCH_DIR` sets the directory (default: the system temporary
//! directory) and `PEPPYDB_BENCH_MILLIS` the time per measurement (default
//! 2000).

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use openraft::AsyncRuntime;
use openraft_rt_tokio::TokioRuntime;
use server_raft_log::LogOptions;
use server_raft_log::RealFs;

use crate::command::Command;
use crate::command::Op;
use crate::disk_log::DiskLog;
use crate::disk_log::Flusher;
use crate::node::RaftConfig;
use crate::node::RaftNode;
use crate::node::Role;
use crate::sim::ModelSm;
use crate::sim::Router;
use crate::types::PeerInfo;

type Rt = TokioRuntime;
type Sm = ModelSm<Rt>;
type Node = RaftNode<Sm, Rt>;

fn measure_time() -> Duration {
    let millis = std::env::var("PEPPYDB_BENCH_MILLIS")
        .ok()
        .map_or(2000, |text| text.parse().expect("a number"));
    Duration::from_millis(millis)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p) as usize]
}

async fn leader(nodes: &[Node]) -> Node {
    loop {
        if let Some(node) = nodes.iter().find(|n| n.status().role == Role::Leader) {
            return node.clone();
        }
        Rt::sleep(Duration::from_millis(10)).await;
    }
}

/// `writers` concurrent clients propose 100-byte writes to the leader of a
/// fresh three-node cluster for the measurement time.
async fn measure(writers: u64) {
    let base = match std::env::var_os("PEPPYDB_BENCH_DIR") {
        Some(base) => tempfile::tempdir_in(base).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let router = Router::new();
    let mut nodes = Vec::new();
    let mut logs = Vec::new();
    for id in 1..=3u64 {
        let dir = base.path().join(id.to_string());
        let log =
            DiskLog::<Rt>::open(RealFs, &dir, LogOptions::default(), Flusher::Thread).unwrap();
        let hook = log.clone();
        let sm = Sm::new(Arc::new(move || drop(hook.sync())));
        let node = Node::start(
            id,
            &RaftConfig::default(),
            log.clone(),
            sm,
            Arc::clone(&router),
        )
        .await
        .unwrap();
        router.register(node.clone());
        logs.push(log);
        nodes.push(node);
    }
    let voters = (1..=3).map(|id| (id, PeerInfo::default())).collect();
    nodes[0].initialize(voters).await.unwrap();
    let first = leader(&nodes).await;
    let term = first.status().term;
    let flushes: u64 = logs.iter().map(DiskLog::flushes).sum();

    let latencies = Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();
    let deadline = started + measure_time();
    let tasks: Vec<_> = (0..writers)
        .map(|w| {
            let (nodes, latencies) = (nodes.clone(), Arc::clone(&latencies));
            Rt::spawn(async move {
                let mut mine = Vec::new();
                let mut n = 0u64;
                while Instant::now() < deadline {
                    let node = leader(&nodes).await;
                    let key = [&[0x01][..], &w.to_le_bytes(), &n.to_le_bytes()].concat();
                    let command = Command {
                        ops: vec![Op::Put {
                            key,
                            value: vec![7; 100],
                        }],
                    };
                    let start = Instant::now();
                    if node.propose(command).await.is_ok() {
                        mine.push(start.elapsed());
                    }
                    n += 1;
                }
                latencies.lock().unwrap().extend(mine);
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let elapsed = started.elapsed();
    let mut latencies = std::mem::take(&mut *latencies.lock().unwrap());
    latencies.sort_unstable();
    let writes = latencies.len() as f64;
    let syncs = logs.iter().map(DiskLog::flushes).sum::<u64>() - flushes;
    let elections = nodes.iter().map(|n| n.status().term).max().unwrap_or(term) - term;
    println!(
        "  {writers:>3} writers: {:>7.0} writes/s, p50 {:>8.1?}, p99 {:>8.1?}, p99.9 {:>8.1?}, {:>5.1} writes per sync per node, {elections} elections",
        writes / elapsed.as_secs_f64(),
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.99),
        percentile(&latencies, 0.999),
        writes * 3.0 / syncs.max(1) as f64,
    );
    for node in &nodes {
        node.shutdown().await;
    }
}

#[test]
#[ignore = "informational benchmark"]
fn bench_three_nodes_on_disk() {
    println!("three nodes, one process, real files, 100-byte writes:");
    for writers in [1, 4, 16, 64, 256] {
        Rt::run(measure(writers));
    }
}
