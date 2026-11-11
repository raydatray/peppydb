//! `SimNetwork`: a [`Transport`] with faults (plan 0028, "The simulator").
//!
//! It delivers through a [`Router`] and, for each message, draws from the
//! seed whether to drop the request, delay it, deliver it twice, or lose
//! the reply. Blocked links model partitions, including one-way ones.
//! Random delays and duplicates also reorder messages.
//!
//! - A **dropped request** never reaches the receiver; the sender sees
//!   `Unreachable` after a delay.
//! - A **lost reply** reaches the receiver, which acts on it, but the
//!   sender sees `Unreachable`: the receiver's state changed without the
//!   sender knowing.
//! - A **duplicate** is delivered again later, and its reply is ignored.
//!   Snapshots are never duplicated.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;

use openraft::AsyncRuntime;
use openraft::storage::RaftStateMachine;

use crate::rng::Rng;
use crate::sim::Probe;
use crate::sim::Router;
use crate::sim::Trace;
use crate::transport::AppendEntries;
use crate::transport::AppendReply;
use crate::transport::SnapshotMeta;
use crate::transport::TransferLeader;
use crate::transport::TransferReply;
use crate::transport::Transport;
use crate::transport::Unreachable;
use crate::transport::Vote;
use crate::transport::VoteReply;
use crate::transport::VoteRequest;
use crate::types::NodeId;
use crate::types::Types;

/// Message fault rates, in percent, and the delay bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Faults {
    pub drop_percent: u64,
    pub lose_reply_percent: u64,
    pub duplicate_percent: u64,
    /// Each delivery waits a delay drawn from zero to this.
    pub max_delay: Duration,
}

impl Faults {
    /// No faults and no delay.
    pub const NONE: Faults = Faults {
        drop_percent: 0,
        lose_reply_percent: 0,
        duplicate_percent: 0,
        max_delay: Duration::ZERO,
    };
}

/// What happens to one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    Blocked,
    Dropped,
    Delivered { lose_reply: bool, duplicate: bool },
}

/// How many messages met each fate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetStats {
    pub delivered: u64,
    pub blocked: u64,
    pub dropped: u64,
    pub lost_replies: u64,
    pub duplicated: u64,
    /// Messages delivered after a delay greater than zero.
    pub delayed: u64,
}

struct State {
    rng: Rng,
    stats: NetStats,
    faults: Faults,
    /// Directed links `(from, to)` that deliver nothing.
    blocked: BTreeSet<(NodeId, NodeId)>,
}

/// A transport with faults over a [`Router`].
pub struct SimNetwork<SM, RT>
where
    RT: AsyncRuntime,
    SM: RaftStateMachine<Types<RT>>,
{
    router: Arc<Router<SM, RT>>,
    state: Mutex<State>,
    trace: Trace,
    probe: Mutex<Option<Arc<dyn Probe>>>,
}

impl<SM, RT> SimNetwork<SM, RT>
where
    RT: AsyncRuntime,
    SM: RaftStateMachine<Types<RT>>,
{
    pub fn new(router: Arc<Router<SM, RT>>, seed: u64, faults: Faults, trace: Trace) -> Arc<Self> {
        Arc::new(Self {
            router,
            state: Mutex::new(State {
                rng: Rng::new(seed),
                stats: NetStats::default(),
                faults,
                blocked: BTreeSet::new(),
            }),
            trace,
            probe: Mutex::new(None),
        })
    }

    /// Show every request and reply to `probe`.
    pub fn set_probe(&self, probe: Arc<dyn Probe>) {
        *self.probe.lock().unwrap_or_else(PoisonError::into_inner) = Some(probe);
    }

    fn probe(&self) -> Option<Arc<dyn Probe>> {
        self.probe
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn router(&self) -> &Arc<Router<SM, RT>> {
        &self.router
    }

    pub fn stats(&self) -> NetStats {
        self.lock().stats
    }

    pub fn set_faults(&self, faults: Faults) {
        self.lock().faults = faults;
    }

    /// Block every link between different groups. Nodes in no group are
    /// isolated.
    pub fn partition(&self, groups: &[BTreeSet<NodeId>], all: &BTreeSet<NodeId>) {
        let group_of = |id: &NodeId| groups.iter().position(|g| g.contains(id));
        let blocked = all
            .iter()
            .flat_map(|a| all.iter().map(move |b| (*a, *b)))
            .filter(|(a, b)| a != b && (group_of(a).is_none() || group_of(a) != group_of(b)))
            .collect();
        self.lock().blocked = blocked;
    }

    /// Block one direction of one link.
    pub fn block(&self, from: NodeId, to: NodeId) {
        self.lock().blocked.insert((from, to));
    }

    /// Unblock every link.
    pub fn heal(&self) {
        self.lock().blocked.clear();
    }

    /// Draw the fate of a message, and its delay.
    fn fate(&self, from: NodeId, to: NodeId, duplicable: bool) -> (Fate, Duration) {
        let mut state = self.lock();
        let max = state.faults.max_delay.as_micros() as u64;
        let delay = Duration::from_micros(match max {
            0 => 0,
            _ => state.rng.below(max + 1),
        });
        let fate = if state.blocked.contains(&(from, to)) {
            Fate::Blocked
        } else {
            let faults = state.faults;
            let mut chance = |percent: u64| percent > 0 && state.rng.below(100) < percent;
            if chance(faults.drop_percent) {
                Fate::Dropped
            } else {
                let lose_reply = chance(faults.lose_reply_percent);
                let duplicate = duplicable && chance(faults.duplicate_percent);
                Fate::Delivered {
                    lose_reply,
                    duplicate,
                }
            }
        };
        let stats = &mut state.stats;
        match fate {
            Fate::Blocked => stats.blocked += 1,
            Fate::Dropped => stats.dropped += 1,
            Fate::Delivered {
                lose_reply,
                duplicate,
            } => {
                stats.delivered += 1;
                stats.lost_replies += u64::from(lose_reply);
                stats.duplicated += u64::from(duplicate);
                stats.delayed += u64::from(!delay.is_zero());
            }
        }
        (fate, delay)
    }

    /// Deliver one message under the faults. `deliver` sends it to the
    /// receiver's node; it runs twice for a duplicate.
    async fn send<Req, Rep, F, Fut>(
        &self,
        kind: &str,
        from: NodeId,
        to: NodeId,
        req: Req,
        duplicable: bool,
        deliver: F,
    ) -> Result<Rep, Unreachable>
    where
        Req: Clone + Send + 'static,
        Rep: Send + 'static,
        F: Fn(Arc<Router<SM, RT>>, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Rep, Unreachable>> + Send + 'static,
    {
        let (fate, delay) = self.fate(from, to, duplicable);
        self.trace.record(format_args!(
            "net {kind} {from}->{to} {fate:?} {}us",
            delay.as_micros()
        ));
        RT::sleep(delay).await;
        match fate {
            Fate::Blocked | Fate::Dropped => Err(Unreachable(format!("{kind} {from}->{to} lost"))),
            Fate::Delivered {
                lose_reply,
                duplicate,
            } => {
                if duplicate {
                    let router = Arc::clone(&self.router);
                    let again = req.clone();
                    let deliver = Arc::new(deliver);
                    let first = Arc::clone(&deliver);
                    // Detached: the duplicate's reply is ignored.
                    let _duplicate = RT::spawn(async move {
                        RT::sleep(delay + Duration::from_millis(1)).await;
                        let _ = deliver(router, again).await;
                    });
                    let reply = first(Arc::clone(&self.router), req).await;
                    return if lose_reply {
                        Err(Unreachable("reply lost".into()))
                    } else {
                        reply
                    };
                }
                let reply = deliver(Arc::clone(&self.router), req).await;
                match lose_reply {
                    true => Err(Unreachable("reply lost".into())),
                    false => reply,
                }
            }
        }
    }
}

fn delivered<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, Unreachable> {
    result.map_err(|error| Unreachable(error.to_string()))
}

impl<SM, RT> Transport for SimNetwork<SM, RT>
where
    RT: AsyncRuntime,
    SM: RaftStateMachine<Types<RT>>,
    SM::SnapshotData: Send,
{
    type SnapshotData = SM::SnapshotData;

    async fn append_entries(
        &self,
        from: NodeId,
        to: NodeId,
        req: AppendEntries,
    ) -> Result<AppendReply, Unreachable> {
        let probe = self.probe();
        if let Some(probe) = &probe {
            probe.request(from, "append", &req.vote);
        }
        self.send("append", from, to, req, true, move |router, req| {
            let probe = probe.clone();
            async move {
                let reply = delivered(router.node(to)?.handle_append_entries(req).await)?;
                if let Some(probe) = &probe {
                    probe.append_reply(to, &reply);
                }
                Ok(reply)
            }
        })
        .await
    }

    async fn vote(
        &self,
        from: NodeId,
        to: NodeId,
        req: VoteRequest,
    ) -> Result<VoteReply, Unreachable> {
        let probe = self.probe();
        if let Some(probe) = &probe {
            probe.request(from, "vote", &req.vote);
        }
        self.send("vote", from, to, req, true, move |router, req| {
            let probe = probe.clone();
            async move {
                let reply = delivered(router.node(to)?.handle_vote(req.clone()).await)?;
                if let Some(probe) = &probe {
                    probe.vote_reply(to, &req, &reply);
                }
                Ok(reply)
            }
        })
        .await
    }

    async fn pre_vote(
        &self,
        from: NodeId,
        to: NodeId,
        req: VoteRequest,
    ) -> Result<VoteReply, Unreachable> {
        self.send("pre-vote", from, to, req, true, move |router, req| async move {
            delivered(router.node(to)?.handle_pre_vote(req).await)
        })
        .await
    }

    async fn snapshot(
        &self,
        from: NodeId,
        to: NodeId,
        vote: Vote,
        meta: SnapshotMeta,
        data: SM::SnapshotData,
    ) -> Result<Vote, Unreachable> {
        let (fate, delay) = self.fate(from, to, false);
        self.trace.record(format_args!(
            "net snapshot {from}->{to} {fate:?} {}us",
            delay.as_micros()
        ));
        RT::sleep(delay).await;
        match fate {
            Fate::Blocked | Fate::Dropped => {
                Err(Unreachable(format!("snapshot {from}->{to} lost")))
            }
            Fate::Delivered { lose_reply, .. } => {
                let reply = delivered(
                    self.router
                        .node(to)?
                        .handle_snapshot(vote, meta, data)
                        .await,
                );
                match lose_reply {
                    true => Err(Unreachable("reply lost".into())),
                    false => reply,
                }
            }
        }
    }

    async fn transfer_leader(
        &self,
        from: NodeId,
        to: NodeId,
        req: TransferLeader,
    ) -> Result<TransferReply, Unreachable> {
        self.send(
            "transfer",
            from,
            to,
            req,
            true,
            move |router, req| async move {
                delivered(router.node(to)?.handle_transfer_leader(req).await)
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use openraft_rt_sim::SimRuntime;

    use super::*;
    use crate::command::Command;
    use crate::command::Op;
    use crate::error::ProposeError;
    use crate::node::RaftConfig;
    use crate::node::RaftNode;
    use crate::sim::LogConfig;
    use crate::sim::MemLog;
    use crate::sim::ModelSm;
    use crate::types::PeerInfo;

    type Sm = ModelSm<SimRuntime>;
    type Node = RaftNode<Sm, SimRuntime>;

    async fn cluster(
        faults: Faults,
    ) -> (
        Arc<SimNetwork<Sm, SimRuntime>>,
        BTreeMap<NodeId, (Node, Sm)>,
    ) {
        let net = SimNetwork::new(Router::new(), 7, faults, Trace::default());
        let mut nodes = BTreeMap::new();
        for id in 1..=3 {
            let log = MemLog::<SimRuntime>::new(LogConfig::default());
            let hook = log.clone();
            let sm = Sm::new(Arc::new(move || hook.sync()));
            let node = Node::start(
                id,
                &RaftConfig::default(),
                log,
                sm.clone(),
                Arc::clone(&net),
            )
            .await
            .unwrap();
            net.router().register(node.clone());
            nodes.insert(id, (node, sm));
        }
        let voters = (1..=3).map(|id| (id, PeerInfo::default())).collect();
        nodes[&1].0.initialize(voters).await.unwrap();
        (net, nodes)
    }

    /// Propose on whichever node the hints point to, until it commits.
    async fn write(nodes: &BTreeMap<NodeId, (Node, Sm)>, key: &[u8], value: &[u8]) -> u64 {
        let command = Command {
            ops: vec![Op::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            }],
        };
        let mut target = 1;
        for _ in 0..200 {
            let attempt = SimRuntime::timeout(
                Duration::from_secs(1),
                nodes[&target].0.propose(command.clone()),
            );
            match attempt.await {
                Ok(Ok(proposed)) => return proposed.log_id.index,
                Ok(Err(ProposeError::NotLeader(hint))) => {
                    target = hint.id.unwrap_or(target % 3 + 1)
                }
                _ => target = target % 3 + 1,
            }
            SimRuntime::sleep(Duration::from_millis(20)).await;
        }
        panic!("no write committed");
    }

    async fn applied_everywhere(nodes: &BTreeMap<NodeId, (Node, Sm)>, index: u64) {
        for _ in 0..2000 {
            if nodes
                .values()
                .all(|(n, _)| n.status().applied.is_some_and(|a| a.index >= index))
            {
                return;
            }
            SimRuntime::sleep(Duration::from_millis(10)).await;
        }
        panic!("index {index} not applied everywhere");
    }

    #[test]
    fn a_cluster_converges_over_a_lossy_network() {
        SimRuntime::with_seed(1).block_on(async {
            let faults = Faults {
                drop_percent: 10,
                lose_reply_percent: 10,
                duplicate_percent: 10,
                max_delay: Duration::from_millis(20),
            };
            let (net, nodes) = cluster(faults).await;
            let mut last = 0;
            for i in 0..30u8 {
                last = write(&nodes, &[1, i % 4], &[i]).await;
            }
            net.set_faults(Faults::NONE);
            applied_everywhere(&nodes, last).await;
            let first = nodes[&1].1.user_contents();
            assert!(nodes.values().all(|(_, sm)| sm.user_contents() == first));
            let stats = net.stats();
            assert!(
                stats.dropped > 0 && stats.lost_replies > 0 && stats.duplicated > 0,
                "{stats:?}"
            );
            for (node, _) in nodes.values() {
                node.shutdown().await;
            }
        });
    }

    #[test]
    fn a_partitioned_node_catches_up_after_healing() {
        SimRuntime::with_seed(2).block_on(async {
            let (net, nodes) = cluster(Faults::NONE).await;
            write(&nodes, b"\x01a", b"1").await;
            let leader = nodes.values().find_map(|(n, _)| n.status().leader).unwrap();
            let follower = (1..=3).find(|id| *id != leader).unwrap();
            let others: BTreeSet<_> = (1..=3).filter(|id| *id != follower).collect();
            net.partition(&[others], &(1..=3).collect());

            let index = write(&nodes, b"\x01b", b"2").await;
            SimRuntime::sleep(Duration::from_secs(1)).await;
            assert_eq!(
                nodes[&follower].1.get(b"\x01b"),
                None,
                "a partitioned node receives nothing"
            );
            assert!(net.stats().blocked > 0);

            net.heal();
            applied_everywhere(&nodes, index).await;
            assert_eq!(nodes[&follower].1.get(b"\x01b"), Some(b"2".to_vec()));
            for (node, _) in nodes.values() {
                node.shutdown().await;
            }
        });
    }
}
