//! Scripted fault scenarios (plan 0028, diff 13). Each runs a quiet cluster
//! (no random faults), applies one fault on purpose, and runs the same
//! safety checks as the seed sweep.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use openraft::AsyncRuntime;
use openraft_rt_sim::SimRuntime;

use crate::command::Command;
use crate::command::Op;
use crate::error::ProposeError;
use crate::node::Role;
use crate::sim::Faults;
use crate::sim::Trace;
use crate::sim::simulator::Cluster;
use crate::sim::simulator::Rt;
use crate::sim::simulator::SimConfig;
use crate::types::NodeId;

fn quiet(nodes: u64, pre_vote: bool) -> SimConfig {
    let mut config = SimConfig::for_seed(1);
    config.nodes = nodes;
    config.faults = Faults::NONE;
    config.chaos = false;
    config.max_flush_delay = Duration::from_millis(1);
    config.raft.pre_vote = pre_vote;
    config
}

fn put(key: &[u8], value: &[u8]) -> Command {
    Command {
        ops: vec![Op::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        }],
    }
}

/// Wait on the virtual clock until `check` holds, for at most `secs`.
async fn eventually(secs: u64, what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..secs * 100 {
        if check() {
            return;
        }
        Rt::sleep(Duration::from_millis(10)).await;
    }
    panic!("not within {secs} s: {what}");
}

async fn leader(cluster: &Cluster) -> NodeId {
    let mut found = None;
    eventually(10, "a leader", || {
        found = cluster.leader();
        found.is_some()
    })
    .await;
    found.unwrap_or_default()
}

fn rest(cluster: &Cluster, except: NodeId) -> BTreeSet<NodeId> {
    cluster
        .ids()
        .into_iter()
        .filter(|id| *id != except)
        .collect()
}

/// Run `body` against a booted quiet cluster, then heal, quiesce, check
/// safety, and shut down.
fn scenario<F>(seed: u64, config: SimConfig, body: impl FnOnce(Arc<Cluster>) -> F)
where
    F: Future<Output = ()>,
{
    SimRuntime::with_seed(seed).block_on(async move {
        let cluster = Cluster::new(seed, config, Trace::default());
        cluster.boot().await;
        body(Arc::clone(&cluster)).await;
        let (contents, violations) = cluster.finish(&[]).await;
        assert!(violations.is_empty(), "{violations:#?}");
        let first = contents.values().next().cloned();
        assert!(
            contents.values().all(|c| Some(c) == first.as_ref()),
            "replicas converge"
        );
        cluster.shutdown().await;
    });
}

#[test]
fn an_isolated_leader_is_replaced_and_rejoins_as_a_follower() {
    scenario(11, quiet(3, true), |cluster| async move {
        let old = leader(&cluster).await;
        let (node, _) = cluster.node(old).unwrap();
        node.propose(put(b"\x01a", b"1")).await.unwrap();
        let old_term = node.status().term;

        cluster
            .net()
            .partition(&[rest(&cluster, old)], &cluster.ids());
        let stuck = Rt::timeout(Duration::from_secs(2), node.propose(put(b"\x01b", b"2"))).await;
        assert!(
            !matches!(stuck, Ok(Ok(_))),
            "an isolated leader cannot commit: {stuck:?}"
        );

        let others: Vec<_> = rest(&cluster, old).into_iter().collect();
        let mut new = None;
        eventually(10, "a new leader among the others", || {
            new = others
                .iter()
                .filter_map(|id| cluster.node(*id))
                .find(|(n, _)| n.status().role == Role::Leader)
                .map(|(n, _)| n);
            new.is_some()
        })
        .await;
        let new = new.unwrap();
        assert!(new.status().term > old_term);
        new.propose(put(b"\x01c", b"3")).await.unwrap();

        cluster.net().heal();
        eventually(10, "the old leader follows", || {
            let status = node.status();
            status.role == Role::Follower && status.leader == Some(new.id())
        })
        .await;
    });
}

/// Isolate a follower for many election timeouts, then heal. Returns the
/// leader and term before and after.
async fn rejoin(cluster: &Cluster) -> ((NodeId, u64), (NodeId, u64), u64) {
    let leader_id = leader(cluster).await;
    let term = cluster.node(leader_id).unwrap().0.status().term;
    let follower = rest(cluster, leader_id).into_iter().next().unwrap();
    cluster
        .net()
        .partition(&[rest(cluster, follower)], &cluster.ids());
    Rt::sleep(Duration::from_secs(5)).await;
    let isolated_term = cluster.node(follower).unwrap().0.status().term;
    cluster.net().heal();
    Rt::sleep(Duration::from_secs(2)).await;
    let after = leader(cluster).await;
    let after_term = cluster.node(after).unwrap().0.status().term;
    ((leader_id, term), (after, after_term), isolated_term)
}

#[test]
fn with_pre_vote_a_rejoining_node_does_not_disrupt_the_leader() {
    scenario(12, quiet(3, true), |cluster| async move {
        let (before, after, isolated_term) = rejoin(&cluster).await;
        assert_eq!(
            isolated_term, before.1,
            "an isolated node does not raise its term"
        );
        assert_eq!(after, before, "the leader and term are unchanged");
    });
}

/// The disruption that pre-vote prevents, shown with pre-vote off.
#[test]
fn without_pre_vote_a_rejoining_node_forces_an_election() {
    scenario(12, quiet(3, false), |cluster| async move {
        let (before, after, isolated_term) = rejoin(&cluster).await;
        assert!(
            isolated_term > before.1,
            "the isolated node raises its term"
        );
        assert!(
            after.1 > before.1,
            "the cluster moves to a higher term: {before:?} -> {after:?}"
        );
    });
}

#[test]
fn a_write_discarded_on_a_leader_change_has_an_unknown_outcome() {
    scenario(13, quiet(3, true), |cluster| async move {
        let old = leader(&cluster).await;
        let (node, _) = cluster.node(old).unwrap();
        cluster
            .net()
            .partition(&[rest(&cluster, old)], &cluster.ids());
        // Appended on the isolated leader only.
        let write = SimRuntime::spawn(async move {
            Rt::timeout(
                Duration::from_secs(30),
                node.propose(put(b"\x01lost", b"x")),
            )
            .await
        });

        let others: Vec<_> = rest(&cluster, old).into_iter().collect();
        eventually(10, "a new leader commits", || {
            others
                .iter()
                .filter_map(|id| cluster.node(*id))
                .any(|(n, _)| n.status().role == Role::Leader && n.status().committed.is_some())
        })
        .await;
        let new = others
            .iter()
            .filter_map(|id| cluster.node(*id))
            .find(|(n, _)| n.status().role == Role::Leader)
            .unwrap()
            .0;
        new.propose(put(b"\x01kept", b"y")).await.unwrap();

        cluster.net().heal();
        let outcome = write.await.unwrap();
        assert!(
            matches!(outcome, Ok(Err(ProposeError::OutcomeUnknown))),
            "the discarded write reports an unknown outcome: {outcome:?}"
        );
        eventually(10, "every node holds the new leader's write", || {
            cluster
                .ids()
                .into_iter()
                .all(|id| cluster.node(id).unwrap().1.get(b"\x01kept").is_some())
        })
        .await;
        for id in cluster.ids() {
            assert_eq!(
                cluster.node(id).unwrap().1.get(b"\x01lost"),
                None,
                "node {id}"
            );
        }
    });
}
