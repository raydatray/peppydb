//! The facade over a perfect network: election, writes, reads, leader
//! hints, and leader transfer (plan 0028, diff 8).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use openraft::AsyncRuntime;
use openraft_rt_sim::SimRuntime;

use crate::command::Command;
use crate::command::Op;
use crate::error::MembershipError;
use crate::error::ProposeError;
use crate::error::ReadError;
use crate::node::RaftConfig;
use crate::node::RaftNode;
use crate::node::Role;
use crate::sim::LogConfig;
use crate::sim::MemLog;
use crate::sim::ModelSm;
use crate::sim::Router;
use crate::types::NodeId;
use crate::types::PeerInfo;

type Sm = ModelSm<SimRuntime>;
type Node = RaftNode<Sm, SimRuntime>;

struct Cluster {
    router: Arc<Router<Sm, SimRuntime>>,
    nodes: BTreeMap<NodeId, (Node, Sm)>,
}

impl Cluster {
    async fn start(ids: &[NodeId]) -> Self {
        let router = Router::new();
        let mut nodes = BTreeMap::new();
        for &id in ids {
            let log = MemLog::<SimRuntime>::new(LogConfig::default());
            let hook = log.clone();
            let sm = Sm::new(Arc::new(move || hook.sync()));
            let node = Node::start(
                id,
                &RaftConfig::default(),
                log,
                sm.clone(),
                Arc::clone(&router),
            )
            .await
            .unwrap();
            router.register(node.clone());
            nodes.insert(id, (node, sm));
        }
        let voters = ids
            .iter()
            .map(|id| {
                (
                    *id,
                    PeerInfo {
                        addr: format!("node-{id}"),
                    },
                )
            })
            .collect();
        nodes[&ids[0]].0.initialize(voters).await.unwrap();
        Self { router, nodes }
    }

    async fn add_unbootstrapped(&mut self, id: NodeId) {
        let log = MemLog::<SimRuntime>::new(LogConfig::default());
        let hook = log.clone();
        let sm = Sm::new(Arc::new(move || hook.sync()));
        let node = Node::start(
            id,
            &RaftConfig::default(),
            log,
            sm.clone(),
            Arc::clone(&self.router),
        )
        .await
        .unwrap();
        self.router.register(node.clone());
        self.nodes.insert(id, (node, sm));
    }

    fn node(&self, id: NodeId) -> &Node {
        &self.nodes[&id].0
    }

    /// Wait on the virtual clock until one node leads and every node agrees.
    async fn leader(&self) -> NodeId {
        for _ in 0..1000 {
            let leaders: Vec<_> = self
                .nodes
                .values()
                .map(|(node, _)| node.status())
                .filter(|status| status.role == Role::Leader)
                .collect();
            if let [leader] = leaders.as_slice() {
                let id = leader.id;
                if self
                    .nodes
                    .values()
                    .all(|(node, _)| node.status().leader == Some(id))
                {
                    return id;
                }
            }
            SimRuntime::sleep(Duration::from_millis(10)).await;
        }
        panic!("no leader after 10 virtual seconds");
    }

    /// Wait until every node has applied through `index`.
    async fn applied_everywhere(&self, index: u64) {
        for _ in 0..1000 {
            let done = self.nodes.values().all(|(node, _)| {
                node.status()
                    .applied
                    .is_some_and(|applied| applied.index >= index)
            });
            if done {
                return;
            }
            SimRuntime::sleep(Duration::from_millis(10)).await;
        }
        panic!("index {index} not applied everywhere");
    }

    async fn shutdown(&self) {
        for (node, _) in self.nodes.values() {
            node.shutdown().await;
        }
    }
}

fn put(key: &[u8], value: &[u8]) -> Command {
    Command {
        ops: vec![Op::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        }],
    }
}

#[test]
fn a_cluster_elects_a_leader_and_replicates_writes() {
    SimRuntime::with_seed(1).block_on(async {
        let cluster = Cluster::start(&[1, 2, 3]).await;
        let leader = cluster.leader().await;

        let mut last = 0;
        for i in 0..20u8 {
            let proposed = cluster
                .node(leader)
                .propose(put(&[1, i], &[i]))
                .await
                .unwrap();
            assert!(proposed.log_id.index > last, "indexes increase");
            last = proposed.log_id.index;
        }
        let (_, leader_sm) = &cluster.nodes[&leader];
        assert_eq!(
            leader_sm.get(&[1, 19]),
            Some(vec![19]),
            "applied on the leader when propose returns"
        );

        cluster.applied_everywhere(last).await;
        let contents: Vec<_> = cluster
            .nodes
            .values()
            .map(|(_, sm)| sm.user_contents())
            .collect();
        assert_eq!(contents[0].len(), 20);
        assert!(
            contents.iter().all(|c| c == &contents[0]),
            "every replica holds the same data"
        );
        cluster.shutdown().await;
    });
}

#[test]
fn followers_refuse_with_a_leader_hint() {
    SimRuntime::with_seed(2).block_on(async {
        let cluster = Cluster::start(&[1, 2, 3]).await;
        let leader = cluster.leader().await;
        let follower = *cluster.nodes.keys().find(|id| **id != leader).unwrap();

        match cluster.node(follower).propose(put(b"\x01k", b"v")).await {
            Err(ProposeError::NotLeader(hint)) => assert_eq!(hint.id, Some(leader)),
            other => panic!("expected NotLeader, got {other:?}"),
        }
        match cluster.node(follower).read_barrier().await {
            Err(ReadError::NotLeader(hint)) => assert_eq!(hint.id, Some(leader)),
            other => panic!("expected NotLeader, got {other:?}"),
        }
        cluster.shutdown().await;
    });
}

#[test]
fn a_read_barrier_covers_every_acknowledged_write() {
    SimRuntime::with_seed(3).block_on(async {
        let cluster = Cluster::start(&[1, 2, 3]).await;
        let leader = cluster.leader().await;
        let proposed = cluster
            .node(leader)
            .propose(put(b"\x01k", b"v"))
            .await
            .unwrap();
        let read = cluster.node(leader).read_barrier().await.unwrap();
        assert!(read.index >= proposed.log_id.index);
        let applied = cluster.node(leader).status().applied.unwrap();
        assert!(
            applied.index >= read.index,
            "applied through the read point"
        );
        cluster.shutdown().await;
    });
}

#[test]
fn leadership_moves_on_request() {
    SimRuntime::with_seed(4).block_on(async {
        let cluster = Cluster::start(&[1, 2, 3]).await;
        let leader = cluster.leader().await;
        let target = *cluster.nodes.keys().find(|id| **id != leader).unwrap();
        let term = cluster.node(leader).status().term;

        cluster.node(leader).transfer_leader(target).await.unwrap();
        for _ in 0..1000 {
            if cluster.node(target).status().role == Role::Leader {
                break;
            }
            SimRuntime::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(cluster.leader().await, target);
        assert!(cluster.node(target).status().term > term);
        cluster
            .node(target)
            .propose(put(b"\x01k", b"v"))
            .await
            .unwrap();
        cluster.shutdown().await;
    });
}

#[test]
fn dynamic_membership_adds_learner_swaps_voter_and_reports_progress() {
    SimRuntime::with_seed(5).block_on(async {
        let mut cluster = Cluster::start(&[1, 2, 3]).await;
        let leader = cluster.leader().await;
        let follower = *cluster.nodes.keys().find(|id| **id != leader).unwrap();

        // Write initial keys so node 4 must catch up.
        let mut last = 0;
        for i in 0..10u8 {
            last = cluster
                .node(leader)
                .propose(put(&[1, i], &[i + 1]))
                .await
                .unwrap()
                .log_id
                .index;
        }

        // Followers reject membership changes with NotLeader + hint.
        let peer4 = PeerInfo {
            addr: "node-4".to_owned(),
        };
        match cluster.node(follower).add_learner(4, peer4.clone()).await {
            Err(MembershipError::NotLeader(hint)) => assert_eq!(hint.id, Some(leader)),
            other => panic!("expected NotLeader, got {other:?}"),
        }

        // Start unbootstrapped node 4 and add it as a non-blocking learner.
        cluster.add_unbootstrapped(4).await;
        let added = cluster
            .node(leader)
            .add_learner(4, peer4.clone())
            .await
            .unwrap();
        assert!(added.log_id.index > last);

        // Wait until leader's Status::replication reports node 4 caught up.
        for _ in 0..1000 {
            let st = cluster.node(leader).status();
            let matched = st
                .replication
                .get(&4)
                .and_then(|opt| opt.map(|id| id.index))
                .unwrap_or(0);
            if matched >= added.log_id.index {
                break;
            }
            SimRuntime::sleep(Duration::from_millis(10)).await;
        }
        let st = cluster.node(leader).status();
        assert_eq!(st.membership.nodes.get(&4), Some(&peer4));
        assert!(
            st.replication
                .get(&4)
                .and_then(|opt| opt.map(|id| id.index))
                .is_some_and(|idx| idx >= added.log_id.index),
            "leader tracks learner match_index"
        );
        assert_eq!(cluster.nodes[&4].1.get(&[1, 9]), Some(vec![10]));

        // Joint-consensus swap: replace `follower` with node 4 in one call.
        let new_voters: BTreeSet<NodeId> = [1, 2, 3, 4]
            .into_iter()
            .filter(|id| *id != follower)
            .collect();
        cluster
            .node(leader)
            .change_membership(new_voters.clone(), false)
            .await
            .unwrap();
        let st = cluster.node(leader).status();
        assert_eq!(st.membership.configs, vec![new_voters.clone()]);
        assert!(!st.membership.nodes.contains_key(&follower));

        // Transfer leadership to the newly promoted node 4 and keep writing.
        cluster.node(leader).transfer_leader(4).await.unwrap();
        for _ in 0..1000 {
            if cluster.node(4).status().role == Role::Leader {
                break;
            }
            SimRuntime::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(cluster.node(4).status().role, Role::Leader);
        cluster
            .node(4)
            .propose(put(b"\x01after-swap", b"ok"))
            .await
            .unwrap();
        assert_eq!(
            cluster.nodes[&4].1.get(b"\x01after-swap"),
            Some(b"ok".to_vec())
        );

        cluster.shutdown().await;
    });
}
