//! The simulator: one seeded run of a cluster under faults (plan 0028,
//! "The simulator").
//!
//! One seed fixes everything: the cluster size, the fault rates, every
//! message's fate and delay, flush delays, crash and partition times, the
//! client workload, and openraft's election timeouts (through
//! `SimRuntime`). A run has three phases:
//!
//! 1. **Chaos.** Clients write and read while a scheduler crashes and
//!    restarts nodes, partitions and heals the network, builds snapshots,
//!    and moves leadership. The network drops, delays, duplicates, and
//!    reorders messages.
//! 2. **Healing.** Every link heals, faults stop, and every node restarts.
//! 3. **Quiescence.** A final write must commit, and every node must apply
//!    it. A cluster that does not recover fails the run.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;

use openraft::AsyncRuntime;
use openraft::Instant;
use openraft_rt_sim::SimInstant;
use openraft_rt_sim::SimRuntime;

use crate::command::Command;
use crate::command::Op;
use crate::error::ProposeError;
use crate::error::ReadError;
use crate::node::RaftConfig;
use crate::node::RaftNode;
use crate::node::Role;
use crate::rng::Rng;
use crate::sim::Contents;
use crate::sim::Faults;
use crate::sim::LogConfig;
use crate::sim::LogImage;
use crate::sim::MemLog;
use crate::sim::ModelSm;
use crate::sim::NetStats;
use crate::sim::Router;
use crate::sim::SimNetwork;
use crate::sim::StoredSnapshot;
use crate::sim::Trace;
use crate::sim::checker::Checker;
use crate::types::NodeId;
use crate::types::PeerInfo;

pub type Rt = SimRuntime;
pub type Sm = ModelSm<Rt>;
pub type Node = RaftNode<Sm, Rt>;
pub type Net = SimNetwork<Sm, Rt>;

/// The shape of one run. [`SimConfig::for_seed`] draws one from a seed.
#[derive(Debug, Clone)]
pub struct SimConfig {
    pub nodes: u64,
    /// Virtual time of the chaos phase.
    pub duration: Duration,
    pub clients: u64,
    /// Clients write and read keys `0x01 ‖ k` for `k` in `0..keys`.
    pub keys: u8,
    pub faults: Faults,
    /// The scheduler acts after a pause drawn from this range.
    pub fault_interval: (Duration, Duration),
    /// Whether the scheduler crashes and partitions; leader transfers and
    /// snapshots run either way.
    pub chaos: bool,
    pub max_flush_delay: Duration,
    pub raft: RaftConfig,
    /// Inject the `MemLog` bug that loses unflushed votes, to show that
    /// the checker catches it.
    pub lazy_votes: bool,
}

impl SimConfig {
    /// A run drawn from `seed`: 3 or 5 nodes, and fault rates that differ
    /// between seeds.
    pub fn for_seed(seed: u64) -> Self {
        let mut rng = Rng::new(seed ^ 0x5eed_c0f1_9000_0000);
        let ms = Duration::from_millis;
        Self {
            nodes: if rng.below(2) == 0 { 3 } else { 5 },
            duration: Duration::from_secs(4),
            clients: 3,
            keys: 4,
            faults: Faults {
                drop_percent: rng.below(8),
                lose_reply_percent: rng.below(8),
                duplicate_percent: rng.below(8),
                max_delay: ms(rng.range(1, 30)),
            },
            fault_interval: (ms(100), ms(600)),
            chaos: true,
            max_flush_delay: ms(rng.range(0, 5)),
            raft: RaftConfig {
                snapshot_after: Some(rng.range(10, 40)),
                keep_below_snapshot: rng.range(0, 10),
                ..RaftConfig::default()
            },
            lazy_votes: false,
        }
    }
}

/// What a client operation returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The write committed at this index.
    Acked(u64),
    /// The write was not proposed.
    Refused,
    /// The write may or may not commit.
    Unknown,
    /// The read barrier passed and the read returned this value.
    Read(Option<Vec<u8>>),
    /// The read barrier failed.
    ReadFailed,
}

/// One client operation, with its virtual start and end times in
/// microseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientOp {
    pub client: u64,
    pub node: NodeId,
    pub key: Vec<u8>,
    /// The value written; `None` for a read.
    pub value: Option<Vec<u8>>,
    pub start: u64,
    pub end: u64,
    pub outcome: Outcome,
}

/// The result of a run.
#[derive(Debug, Clone)]
pub struct Report {
    pub seed: u64,
    pub config: SimConfig,
    pub trace_hash: u64,
    pub trace_events: u64,
    pub history: Vec<ClientOp>,
    /// Each node's user data after quiescence.
    pub contents: BTreeMap<NodeId, Contents>,
    pub net: NetStats,
    pub crashes: u64,
    pub partitions: u64,
    pub transfers: u64,
    pub snapshots: u64,
    /// How many times a node became leader.
    pub elections: u64,
    pub violations: Vec<String>,
}

impl Report {
    /// Panic with every violation and the command that reproduces the run.
    pub fn assert_safe(&self) {
        if self.violations.is_empty() {
            return;
        }
        panic!(
            "seed {} violates safety:\n  {}\nreproduce: PEPPY_SIM_SEED={} tools/buck2 run //lib/server-raft:server-raft-unittest -- tests::sweep::one_seed --nocapture",
            self.seed,
            self.violations.join("\n  "),
            self.seed
        );
    }
}

/// Microseconds of virtual time since the runtime started.
pub fn now() -> u64 {
    SimInstant::now().elapsed_since_start().as_micros() as u64
}

/// A node's storage, which outlives crashes, and its running instance.
struct Slot {
    log: MemLog<Rt>,
    sm: Sm,
    node: Option<Node>,
}

/// The cluster: every node's slot, and the shared network.
pub struct Cluster {
    slots: Mutex<BTreeMap<NodeId, Slot>>,
    net: Arc<Net>,
    config: SimConfig,
    rng: Mutex<Rng>,
    trace: Trace,
    checker: Arc<Checker>,
}

impl Cluster {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<NodeId, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn ids(&self) -> BTreeSet<NodeId> {
        (1..=self.config.nodes).collect()
    }

    pub fn net(&self) -> &Arc<Net> {
        &self.net
    }

    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    /// The running node and its state machine, if `id` is up.
    pub fn node(&self, id: NodeId) -> Option<(Node, Sm)> {
        let slots = self.lock();
        let slot = slots.get(&id)?;
        slot.node.clone().map(|node| (node, slot.sm.clone()))
    }

    pub fn up(&self) -> Vec<NodeId> {
        self.lock()
            .iter()
            .filter(|(_, slot)| slot.node.is_some())
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn down(&self) -> Vec<NodeId> {
        self.lock()
            .iter()
            .filter(|(_, slot)| slot.node.is_none())
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn log(&self, id: NodeId) -> MemLog<Rt> {
        self.lock()[&id].log.clone()
    }

    pub fn checker(&self) -> &Arc<Checker> {
        &self.checker
    }

    /// Every node's log: live if it is up, durable if it is down.
    pub fn images(&self) -> BTreeMap<NodeId, LogImage> {
        self.lock()
            .iter()
            .map(|(id, slot)| {
                let image = match slot.node {
                    Some(_) => slot.log.live(),
                    None => slot.log.durable(),
                };
                (*id, image)
            })
            .collect()
    }

    /// Run the log matching check now.
    pub fn check_logs(&self) {
        self.checker.check_logs(now(), &self.images());
    }

    /// Start `id` over storage restored from `image` and `durable`.
    async fn start(&self, id: NodeId, image: LogImage, durable: Option<StoredSnapshot>) -> Node {
        let seed = self
            .rng
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next_u64();
        let log_config = LogConfig {
            max_flush_delay: self.config.max_flush_delay,
            seed,
            lazy_votes: self.config.lazy_votes,
        };
        let log = MemLog::from_image(image, log_config);
        let hook = log.clone();
        let sm = Sm::from_durable(durable, Arc::new(move || hook.sync()));
        log.observe(id, Arc::clone(&self.checker) as _);
        sm.observe(id, Arc::clone(&self.checker) as _);
        self.checker.register(id, log.clone(), sm.clone());
        let node = Node::start(
            id,
            &self.config.raft,
            log.clone(),
            sm.clone(),
            Arc::clone(&self.net),
        )
        .await
        .unwrap_or_else(|error| panic!("node {id} does not start: {error}"));
        self.net.router().register(node.clone());
        self.lock().insert(
            id,
            Slot {
                log,
                sm,
                node: Some(node.clone()),
            },
        );
        node
    }

    /// Crash `id`: storage loses everything that is not durable, and the
    /// node stops.
    pub async fn crash(&self, id: NodeId) {
        let node = {
            let mut slots = self.lock();
            let slot = slots.get_mut(&id).expect("a known node");
            let Some(node) = slot.node.take() else { return };
            slot.log.crash();
            slot.sm.crash();
            node
        };
        self.net.router().unregister(id);
        node.shutdown().await;
        self.trace.record(format_args!("{} crash {id}", now()));
    }

    /// Restart a crashed node from its durable storage.
    pub async fn restart(&self, id: NodeId) {
        let (image, durable) = {
            let slots = self.lock();
            let slot = &slots[&id];
            if slot.node.is_some() {
                return;
            }
            (slot.log.durable(), slot.sm.durable())
        };
        self.start(id, image, durable).await;
        self.trace.record(format_args!("{} restart {id}", now()));
    }

    /// The node that every up node agrees leads, if any.
    pub fn leader(&self) -> Option<NodeId> {
        let statuses: Vec<_> = self
            .up()
            .into_iter()
            .filter_map(|id| self.node(id))
            .map(|(n, _)| n.status())
            .collect();
        let leaders: Vec<_> = statuses
            .iter()
            .filter(|s| s.role == Role::Leader)
            .map(|s| s.id)
            .collect();
        match leaders.as_slice() {
            [leader] if statuses.iter().all(|s| s.leader == Some(*leader)) => Some(*leader),
            _ => None,
        }
    }

    pub async fn shutdown(&self) {
        for id in self.up() {
            if let Some((node, _)) = self.node(id) {
                node.shutdown().await;
            }
        }
    }
}

impl Cluster {
    /// A cluster for `seed` and `config` whose nodes are not started yet.
    /// Call it inside the seed's `SimRuntime`.
    pub fn new(seed: u64, config: SimConfig, trace: Trace) -> Arc<Self> {
        let mut rng = Rng::new(seed);
        let net = SimNetwork::new(Router::new(), rng.next_u64(), config.faults, trace.clone());
        let checker = Arc::new(Checker::default());
        net.set_probe(Arc::clone(&checker) as _);
        trace.record(format_args!("seed {seed} config {config:?}"));
        Arc::new(Cluster {
            slots: Mutex::new(BTreeMap::new()),
            net,
            config,
            rng: Mutex::new(rng),
            trace,
            checker,
        })
    }

    /// Start every node and form the cluster.
    pub async fn boot(&self) {
        for id in self.ids() {
            self.start(id, LogImage::default(), None).await;
        }
        let voters: BTreeMap<_, _> = self
            .ids()
            .into_iter()
            .map(|id| (id, PeerInfo::default()))
            .collect();
        let (first, _) = self.node(1).expect("node 1 is up");
        first
            .initialize(voters)
            .await
            .expect("a new cluster initializes");
    }

    /// A random stream derived from the cluster's seed.
    fn fork_rng(&self) -> Rng {
        self.rng
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fork()
    }

    /// Heal, wait for quiescence, run the final checks, and return each
    /// node's user data and the violations.
    pub async fn finish(&self, history: &[ClientOp]) -> (BTreeMap<NodeId, Contents>, Vec<String>) {
        quiesce(self).await;
        let contents = self
            .ids()
            .into_iter()
            .map(|id| {
                (
                    id,
                    self.node(id).expect("up after healing").1.user_contents(),
                )
            })
            .collect();
        self.check_logs();
        self.checker.check_history(history, &contents);
        (contents, self.checker.violations())
    }
}

/// Run one simulation for `seed` with `config`, calling `observe` with the
/// cluster after quiescence and before shutdown.
pub fn run_with(
    seed: u64,
    config: SimConfig,
    trace: Trace,
    observe: impl FnOnce(&Cluster, &Report),
) -> Report {
    let mut rt = SimRuntime::with_seed(seed);
    rt.block_on(async move {
        let cluster = Cluster::new(seed, config.clone(), trace.clone());
        cluster.boot().await;

        let deadline = now() + config.duration.as_micros() as u64;
        let history = Arc::new(Mutex::new(Vec::new()));
        let clients: Vec<_> = (0..config.clients)
            .map(|client| {
                let cluster = Arc::clone(&cluster);
                let history = Arc::clone(&history);
                let rng = cluster.fork_rng();
                SimRuntime::spawn(client_loop(client, rng, cluster, history, deadline))
            })
            .collect();
        let stats = scheduler(&cluster, cluster.fork_rng(), deadline).await;
        for client in clients {
            client.await.expect("a client task panicked");
        }

        let history = std::mem::take(&mut *history.lock().unwrap_or_else(PoisonError::into_inner));
        let (contents, violations) = cluster.finish(&history).await;
        let report = Report {
            seed,
            config,
            trace_hash: trace.hash(),
            trace_events: trace.len(),
            history,
            contents,
            net: cluster.net.stats(),
            crashes: stats.crashes,
            partitions: stats.partitions,
            transfers: stats.transfers,
            snapshots: stats.snapshots,
            elections: cluster.checker.elections(),
            violations,
        };
        observe(&cluster, &report);
        cluster.shutdown().await;
        report
    })
}

/// Run one simulation drawn from `seed`.
pub fn run(seed: u64) -> Report {
    run_with(seed, SimConfig::for_seed(seed), Trace::default(), |_, _| {})
}

#[derive(Default)]
struct ChaosStats {
    crashes: u64,
    partitions: u64,
    transfers: u64,
    snapshots: u64,
}

fn pick<T: Copy>(rng: &mut Rng, items: &[T]) -> Option<T> {
    match items.len() {
        0 => None,
        n => Some(items[rng.below(n as u64) as usize]),
    }
}

/// Inject faults until the deadline.
async fn scheduler(cluster: &Cluster, mut rng: Rng, deadline: u64) -> ChaosStats {
    let config = &cluster.config;
    let mut stats = ChaosStats::default();
    let all = cluster.ids();
    let minority = (config.nodes - 1) / 2;
    while now() < deadline {
        let (lo, hi) = config.fault_interval;
        let pause = rng.range(lo.as_micros() as u64, hi.as_micros() as u64);
        Rt::sleep(Duration::from_micros(pause)).await;
        cluster.check_logs();
        let roll = rng.below(100);
        let up = cluster.up();
        let down = cluster.down();
        match roll {
            0..25 if config.chaos && (down.len() as u64) < minority.max(1) => {
                if let Some(id) = pick(&mut rng, &up) {
                    cluster.crash(id).await;
                    stats.crashes += 1;
                }
            }
            0..45 => {
                if let Some(id) = pick(&mut rng, &down) {
                    cluster.restart(id).await;
                }
            }
            45..60 if config.chaos => {
                let shape = rng.below(4);
                let ids: Vec<_> = all.iter().copied().collect();
                match shape {
                    // Isolate the leader, or a random node.
                    0 => {
                        let target = cluster
                            .leader()
                            .or_else(|| pick(&mut rng, &ids))
                            .unwrap_or(1);
                        let rest: BTreeSet<_> =
                            all.iter().copied().filter(|id| *id != target).collect();
                        cluster.net.partition(&[rest], &all);
                        cluster
                            .trace
                            .record(format_args!("{} isolate {target}", now()));
                    }
                    // Split into a minority and a majority at random.
                    1 => {
                        let mut shuffled = ids.clone();
                        for i in (1..shuffled.len()).rev() {
                            shuffled.swap(i, rng.below(i as u64 + 1) as usize);
                        }
                        let cut = (config.nodes / 2) as usize;
                        let a: BTreeSet<_> = shuffled[..cut].iter().copied().collect();
                        let b: BTreeSet<_> = shuffled[cut..].iter().copied().collect();
                        cluster
                            .trace
                            .record(format_args!("{} split {a:?} {b:?}", now()));
                        cluster.net.partition(&[a, b], &all);
                    }
                    // Block one direction of one link.
                    2 => {
                        let from = pick(&mut rng, &ids).unwrap_or(1);
                        let to = pick(&mut rng, &ids).unwrap_or(2);
                        if from != to {
                            cluster.net.block(from, to);
                            cluster
                                .trace
                                .record(format_args!("{} block {from}->{to}", now()));
                        }
                    }
                    // Every node alone.
                    _ => {
                        cluster.net.partition(&[], &all);
                        cluster.trace.record(format_args!("{} isolate all", now()));
                    }
                }
                stats.partitions += 1;
            }
            45..75 => {
                cluster.net.heal();
                cluster.trace.record(format_args!("{} heal", now()));
            }
            75..88 => {
                if let Some((node, _)) = pick(&mut rng, &up).and_then(|id| cluster.node(id)) {
                    let _ = node.snapshot().await;
                    cluster
                        .trace
                        .record(format_args!("{} snapshot {}", now(), node.id()));
                    stats.snapshots += 1;
                }
            }
            _ => {
                let leader = cluster.leader().and_then(|id| cluster.node(id));
                if let (Some((node, _)), Some(to)) = (leader, pick(&mut rng, &up))
                    && to != node.id()
                {
                    let _ = node.transfer_leader(to).await;
                    cluster
                        .trace
                        .record(format_args!("{} transfer {}->{to}", now(), node.id()));
                    stats.transfers += 1;
                }
            }
        }
    }
    stats
}

/// Heal everything and wait for a final write to commit on every node.
async fn quiesce(cluster: &Cluster) {
    cluster.net.heal();
    cluster.net.set_faults(Faults::NONE);
    for id in cluster.down() {
        cluster.restart(id).await;
    }
    cluster.trace.record(format_args!("{} quiesce", now()));

    let give_up = now() + 60_000_000;
    let index = loop {
        assert!(
            now() < give_up,
            "no leader commits a write within 60 s of healing"
        );
        Rt::sleep(Duration::from_millis(50)).await;
        let Some((node, _)) = cluster.leader().and_then(|id| cluster.node(id)) else {
            continue;
        };
        let write = Command {
            ops: vec![Op::Put {
                key: b"\x01final".to_vec(),
                value: b"done".to_vec(),
            }],
        };
        match Rt::timeout(Duration::from_secs(1), node.propose(write)).await {
            Ok(Ok(proposed)) => break proposed.log_id.index,
            Ok(Err(_)) | Err(_) => continue,
        }
    };
    loop {
        assert!(
            now() < give_up,
            "index {index} is not applied on every node within 60 s of healing"
        );
        let applied = cluster.ids().into_iter().all(|id| {
            cluster
                .node(id)
                .and_then(|(node, _)| node.status().applied)
                .is_some_and(|applied| applied.index >= index)
        });
        if applied {
            break;
        }
        Rt::sleep(Duration::from_millis(20)).await;
    }
    cluster
        .trace
        .record(format_args!("{} quiescent at {index}", now()));
}

/// One client: random writes of unique values and linearizable reads until
/// the deadline. It usually sends to the node that the last leader hint
/// named, and sometimes to a random node.
async fn client_loop(
    client: u64,
    mut rng: Rng,
    cluster: Arc<Cluster>,
    history: Arc<Mutex<Vec<ClientOp>>>,
    deadline: u64,
) {
    let mut seq = 0u64;
    let mut guess: Option<NodeId> = None;
    while now() < deadline {
        Rt::sleep(Duration::from_micros(rng.below(20_000))).await;
        let id = match guess {
            Some(id) if rng.below(100) < 80 => id,
            _ => rng.range(1, cluster.config.nodes),
        };
        let key = vec![0x01, rng.below(u64::from(cluster.config.keys)) as u8];
        let Some((node, sm)) = cluster.node(id) else {
            continue;
        };
        let start = now();
        let (value, outcome) = if rng.below(100) < 60 {
            seq += 1;
            let value = format!("c{client}-{seq}").into_bytes();
            let command = Command {
                ops: vec![Op::Put {
                    key: key.clone(),
                    value: value.clone(),
                }],
            };
            let outcome = match Rt::timeout(Duration::from_secs(2), node.propose(command)).await {
                Ok(Ok(proposed)) => Outcome::Acked(proposed.log_id.index),
                Ok(Err(ProposeError::NotLeader(hint))) => {
                    guess = hint.id;
                    Outcome::Refused
                }
                Ok(Err(_)) | Err(_) => Outcome::Unknown,
            };
            (Some(value), outcome)
        } else {
            let outcome = match Rt::timeout(Duration::from_secs(2), node.read_barrier()).await {
                // Nothing awaits between the barrier and the read, so no
                // entry applies in between on this single-threaded runtime.
                Ok(Ok(_)) => Outcome::Read(sm.get(&key)),
                Ok(Err(ReadError::NotLeader(hint))) => {
                    guess = hint.id;
                    Outcome::ReadFailed
                }
                Ok(Err(_)) | Err(_) => Outcome::ReadFailed,
            };
            (None, outcome)
        };
        let op = ClientOp {
            client,
            node: id,
            key,
            value,
            start,
            end: now(),
            outcome,
        };
        cluster
            .trace
            .record(format_args!("{} client {op:?}", op.end));
        history
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(op);
    }
}
