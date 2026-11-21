//! Informational benchmark of three nodes on real files, not a gate (plans
//! 0029 and 0030). The nodes run in one process over a perfect in-memory
//! network. Run in release mode:
//!
//! ```text
//! tools/buck2 run @build/mode/release //lib/server-raft:server-raft-unittest -- --ignored --nocapture tests::bench::
//! ```
//!
//! Three configurations:
//!
//! - **shared, model:** every node and the clients on one Tokio runtime,
//!   over `ModelSm` (plan 0029's setup).
//! - **per node, model:** each node on its own Tokio runtime, as it would
//!   be in its own process; the clients on another.
//! - **per node, engine:** the same with `DbStateMachine` over an
//!   external-mode `engine::Db` and its applier thread (plan 0030).
//!
//! Besides throughput and latency, each run reports elections and **timer
//! lateness**: a probe on each node's runtime sleeps 10 ms in a loop and
//! records how late it wakes. openraft's heartbeats and election timers run
//! on the same runtime, so lateness is how much load delays them.
//!
//! The nodes share one machine and one disk. `PEPPYDB_BENCH_DIR` sets the
//! directory (default: the system temporary directory),
//! `PEPPYDB_BENCH_MILLIS` the time per measurement (default 2000), and
//! `PEPPYDB_BENCH_THREADS` each node runtime's worker threads (default: a
//! quarter of the cores, at least one, so three nodes and the clients do
//! not oversubscribe the machine; 3 on a 12-core laptop, 2 on GCP's
//! 8-vCPU `c4d-standard-8-lssd`).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use openraft::AsyncRuntime;
use openraft::storage::RaftStateMachine;
use openraft_rt_tokio::TokioRuntime;
use server_raft_log::LogOptions;
use server_raft_log::RealFs;

use crate::command::Command;
use crate::command::Op;
use crate::db_sm::Applier;
use crate::db_sm::DbStateMachine;
use crate::disk_log::DiskLog;
use crate::disk_log::Flusher;
use crate::node::RaftConfig;
use crate::node::RaftNode;
use crate::node::Role;
use crate::sim::ModelSm;
use crate::sim::Router;
use crate::types::NodeId;
use crate::types::PeerInfo;
use crate::types::Types;

type Rt = TokioRuntime;

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .map_or(default, |text| text.parse().expect("a number"))
}

/// Worker threads per node runtime: `PEPPYDB_BENCH_THREADS`, or a quarter
/// of the cores.
fn node_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    env("PEPPYDB_BENCH_THREADS", (cores / 4).max(1) as u64) as usize
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted
        .get(((sorted.len().max(1) - 1) as f64 * p) as usize)
        .copied()
        .unwrap_or_default()
}

/// How a node's state machine is built over its log.
trait Machine:
    RaftStateMachine<Types<Rt>, SnapshotData = Vec<u8>> + Clone + Send + Sync + 'static
{
    fn build(dir: &std::path::Path, log: &DiskLog<Rt>) -> Self;
}

impl Machine for ModelSm<Rt> {
    fn build(_dir: &std::path::Path, log: &DiskLog<Rt>) -> Self {
        let log = log.clone();
        ModelSm::new(Arc::new(move || drop(log.sync())))
    }
}

impl Machine for DbStateMachine<Rt> {
    fn build(dir: &std::path::Path, log: &DiskLog<Rt>) -> Self {
        let options = engine::Options {
            mode: engine::Mode::External,
            ..engine::Options::default()
        };
        let db = engine::Db::open(dir.join("db"), options).unwrap();
        let log = log.clone();
        DbStateMachine::open(db, Arc::new(move || log.sync()), Applier::Thread).unwrap()
    }
}

async fn start<SM: Machine>(
    id: NodeId,
    dir: &std::path::Path,
    router: &Arc<Router<SM, Rt>>,
) -> RaftNode<SM, Rt> {
    let log = DiskLog::<Rt>::open(
        RealFs,
        &dir.join("log"),
        LogOptions::default(),
        Flusher::Thread,
    )
    .unwrap();
    let sm = SM::build(dir, &log);
    let node = RaftNode::start(id, &RaftConfig::default(), log, sm, Arc::clone(router))
        .await
        .unwrap();
    router.register(node.clone());
    node
}

/// Sleep 10 ms in a loop until `stop`, recording how late each wake is.
async fn probe(stop: Arc<AtomicBool>) -> Vec<Duration> {
    let tick = Duration::from_millis(10);
    let mut late = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let before = Instant::now();
        Rt::sleep(tick).await;
        late.push(before.elapsed().saturating_sub(tick));
    }
    late
}

async fn leader<SM: Machine>(nodes: &[RaftNode<SM, Rt>]) -> RaftNode<SM, Rt> {
    loop {
        if let Some(node) = nodes.iter().find(|n| n.status().role == Role::Leader) {
            return node.clone();
        }
        Rt::sleep(Duration::from_millis(10)).await;
    }
}

/// `writers` clients propose 100-byte writes to the leader for the
/// measurement time. Returns each write's latency.
async fn drive<SM: Machine>(
    nodes: Vec<RaftNode<SM, Rt>>,
    writers: u64,
) -> (Vec<Duration>, Duration) {
    let latencies = Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();
    let deadline = started + Duration::from_millis(env("PEPPYDB_BENCH_MILLIS", 2000));
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
    (latencies, elapsed)
}

struct Run {
    latencies: Vec<Duration>,
    elapsed: Duration,
    late: Vec<Duration>,
    elections: u64,
    snapshots: u64,
}

fn report(label: &str, writers: u64, mut run: Run) {
    run.late.sort_unstable();
    println!(
        "  {label:<17} {writers:>3} writers: {:>7.0} writes/s, p50 {:>7.1?}, p99 {:>7.1?}, timer late p99 {:>6.1?} max {:>6.1?}, {} elections, {} snapshots",
        run.latencies.len() as f64 / run.elapsed.as_secs_f64(),
        percentile(&run.latencies, 0.5),
        percentile(&run.latencies, 0.99),
        percentile(&run.late, 0.99),
        run.late.last().copied().unwrap_or_default(),
        run.elections,
        run.snapshots,
    );
}

fn base() -> tempfile::TempDir {
    match std::env::var_os("PEPPYDB_BENCH_DIR") {
        Some(base) => tempfile::tempdir_in(base).unwrap(),
        None => tempfile::tempdir().unwrap(),
    }
}

/// Elections during the run, and snapshots built.
fn after<SM: Machine>(nodes: &[RaftNode<SM, Rt>], term: u64) -> (u64, u64) {
    let statuses: Vec<_> = nodes.iter().map(RaftNode::status).collect();
    let elections = statuses.iter().map(|s| s.term).max().unwrap_or(term) - term;
    let snapshots = statuses.iter().filter(|s| s.snapshot.is_some()).count() as u64;
    (elections, snapshots)
}

/// Every node and the clients on one runtime.
fn shared<SM: Machine>(writers: u64) -> Run {
    let dir = base();
    Rt::new(node_threads() * 3).block_on(async move {
        let router = Router::new();
        let mut nodes = Vec::new();
        for id in 1..=3 {
            nodes.push(start::<SM>(id, &dir.path().join(id.to_string()), &router).await);
        }
        nodes[0]
            .initialize((1..=3).map(|id| (id, PeerInfo::default())).collect())
            .await
            .unwrap();
        let term = leader(&nodes).await.status().term;
        let stop = Arc::new(AtomicBool::new(false));
        let probe = Rt::spawn(probe(Arc::clone(&stop)));
        let (latencies, elapsed) = drive(nodes.clone(), writers).await;
        stop.store(true, Ordering::Relaxed);
        let late = probe.await.unwrap();
        let (elections, snapshots) = after(&nodes, term);
        for node in &nodes {
            node.shutdown().await;
        }
        Run {
            latencies,
            elapsed,
            late,
            elections,
            snapshots,
        }
    })
}

/// Each node on its own runtime, on its own thread; the clients on the
/// caller's.
fn per_node<SM: Machine>(writers: u64) -> Run {
    let dir = base();
    let router: Arc<Router<SM, Rt>> = Router::new();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let hosts: Vec<_> = (1..=3u64)
        .map(|id| {
            let (dir, router, stop, tx) = (
                dir.path().join(id.to_string()),
                Arc::clone(&router),
                Arc::clone(&stop),
                tx.clone(),
            );
            std::thread::spawn(move || {
                Rt::new(node_threads()).block_on(async move {
                    let node = start::<SM>(id, &dir, &router).await;
                    tx.send(node.clone()).unwrap();
                    let late = probe(stop).await;
                    node.shutdown().await;
                    late
                })
            })
        })
        .collect();
    let mut nodes: Vec<_> = (0..3).map(|_| rx.recv().unwrap()).collect();
    nodes.sort_by_key(RaftNode::id);
    let (latencies, elapsed, elections, snapshots) = Rt::new(2).block_on(async move {
        nodes[0]
            .initialize((1..=3).map(|id| (id, PeerInfo::default())).collect())
            .await
            .unwrap();
        let term = leader(&nodes).await.status().term;
        let (latencies, elapsed) = drive(nodes.clone(), writers).await;
        let (elections, snapshots) = after(&nodes, term);
        (latencies, elapsed, elections, snapshots)
    });
    stop.store(true, Ordering::Relaxed);
    let late = hosts
        .into_iter()
        .flat_map(|host| host.join().unwrap())
        .collect();
    Run {
        latencies,
        elapsed,
        late,
        elections,
        snapshots,
    }
}

#[test]
#[ignore = "informational benchmark"]
fn bench_three_nodes_on_disk() {
    println!("three nodes, one process, real files, 100-byte writes:");
    for writers in [1, 16, 64, 256] {
        report("shared, model", writers, shared::<ModelSm<Rt>>(writers));
        report("per node, model", writers, per_node::<ModelSm<Rt>>(writers));
        report(
            "per node, engine",
            writers,
            per_node::<DbStateMachine<Rt>>(writers),
        );
    }
}

/// `Db::apply` alone, one second each, with keys that stay in one set or
/// keep growing the tree, with and without another thread syncing a file
/// on the same disk. Explains the engine runs above: on macOS, applies
/// that grow the tree wait behind other files' full flushes (plan 0030,
/// discoveries).
#[test]
#[ignore = "informational benchmark"]
fn bench_engine_apply_beside_a_syncing_file() {
    println!("Db::apply of a 100-byte put and the applied log id:");
    for growing in [false, true] {
        for syncing in [false, true] {
            let dir = base();
            let options = engine::Options {
                mode: engine::Mode::External,
                ..engine::Options::default()
            };
            let db = engine::Db::open(dir.path().join("db"), options).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let syncer = {
                let (stop, path) = (Arc::clone(&stop), dir.path().join("other"));
                std::thread::spawn(move || {
                    use std::io::Write;
                    let mut file = std::fs::File::create(path).unwrap();
                    let mut syncs = 0;
                    while syncing && !stop.load(Ordering::Relaxed) {
                        file.write_all(&[1; 128]).unwrap();
                        file.sync_data().unwrap();
                        syncs += 1;
                    }
                    syncs
                })
            };
            let started = Instant::now();
            let mut seq = 0u64;
            while started.elapsed() < Duration::from_secs(1) {
                seq += 1;
                let key = if growing { seq } else { seq % 100 };
                let key = [&[0x01][..], &key.to_le_bytes()].concat();
                let ops = [
                    engine::Op::Put {
                        key: &key,
                        value: &[7; 100],
                    },
                    engine::Op::Put {
                        key: server_keys::APPLIED_KEY,
                        value: &[1; 16],
                    },
                ];
                db.apply(seq, &ops).unwrap();
            }
            stop.store(true, Ordering::Relaxed);
            let keys = if growing {
                "growing keys"
            } else {
                "100 keys    "
            };
            let other = if syncing {
                "beside a syncing file"
            } else {
                "alone                "
            };
            println!(
                "  {keys} {other}: {:>6.1} µs per apply ({} syncs beside)",
                started.elapsed().as_secs_f64() * 1e6 / seq as f64,
                syncer.join().unwrap()
            );
        }
    }
}
