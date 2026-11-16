//! The facade over a perfect network: election, writes, reads, leader
//! hints, and leader transfer (plan 0028, diff 8).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use openraft::AsyncRuntime;
use openraft_rt_sim::SimRuntime;

use crate::command::Command;
use crate::command::Op;
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
        let voters = ids.iter().map(|id| (*id, PeerInfo::default())).collect();
        nodes[&ids[0]].0.initialize(voters).await.unwrap();
        Self { nodes }
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
