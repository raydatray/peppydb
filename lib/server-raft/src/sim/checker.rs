//! The safety checker (plan 0028, "Safety checkers"; plan 0025, decision
//! 15).
//!
//! It observes the storage models and the network during a run, and the
//! client history and replicas after it. Every check records a violation
//! rather than panicking, so a run reports all of them with its seed.
//!
//! | Property | How it is observed |
//! | --- | --- |
//! | Election safety (paper §5.2) | Every committed vote that any node saves for a term names the same leader. |
//! | Leader completeness (§5.4.1) | When a node saves a committed vote for itself, its log holds every entry applied anywhere so far, except below its purge point. |
//! | State machine safety (§5.4.3) | One index never applies two different entries. |
//! | Log matching (§5.3) | Two logs that hold the same log id hold the same entries up to it. Checked after every scheduler step. |
//! | Persist before send (decision 7) | A sender's vote is durable when it sends; a granted vote and an accepted append are durable on the receiver when it replies. |
//! | Acknowledged writes survive | Each acknowledged write is the entry applied at its index. |
//! | Replicas equal the apply order | Replaying every applied entry in index order gives each replica's contents. |
//! | Read-index reads | A read returns a committed value, not one written after the read ended, and not one older than a write acknowledged before the read started. |

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use openraft::vote::RaftLeaderId;

use crate::command::Op;
use crate::sim::Contents;
use crate::sim::LogImage;
use crate::sim::MemLog;
use crate::sim::Observer;
use crate::sim::Probe;
use crate::sim::simulator::ClientOp;
use crate::sim::simulator::Outcome;
use crate::sim::simulator::Rt;
use crate::sim::simulator::Sm;
use crate::transport::AppendReply;
use crate::transport::Vote;
use crate::transport::VoteReply;
use crate::transport::VoteRequest;
use crate::types::NodeId;
use crate::types::OrEntry;
use crate::types::OrPayload;
use crate::types::OrVote;

#[derive(Default)]
struct State {
    /// The leader of each term, from committed votes.
    leaders: BTreeMap<u64, NodeId>,
    /// How many times a node saved a committed vote for itself.
    elections: u64,
    /// Every applied entry, by index.
    applied: BTreeMap<u64, OrEntry>,
    /// Each node's current log and state machine, for durable-state probes.
    logs: BTreeMap<NodeId, MemLog<Rt>>,
    sms: BTreeMap<NodeId, Sm>,
    violations: Vec<String>,
}

/// Observes one run and records violations.
#[derive(Default)]
pub struct Checker {
    state: Mutex<State>,
}

/// Whether `durable` is `vote` or a later vote.
fn covers(durable: Option<OrVote>, vote: OrVote) -> bool {
    durable.is_some_and(|durable| durable >= vote)
}

impl Checker {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn violation(state: &mut State, message: String) {
        // Keep the first few; one bug tends to repeat.
        if state.violations.len() < 20 {
            state.violations.push(message);
        }
    }

    pub fn violations(&self) -> Vec<String> {
        self.lock().violations.clone()
    }

    pub fn elections(&self) -> u64 {
        self.lock().elections
    }

    /// Record that `node` now runs over `log` and `sm`.
    pub fn register(&self, node: NodeId, log: MemLog<Rt>, sm: Sm) {
        let mut state = self.lock();
        state.logs.insert(node, log);
        state.sms.insert(node, sm);
    }

    fn durable(&self, node: NodeId) -> Option<LogImage> {
        let log = self.lock().logs.get(&node).cloned()?;
        Some(log.durable())
    }

    fn report(&self, message: String) {
        Self::violation(&mut self.lock(), message);
    }

    /// Log matching over every node's log: live for an up node, durable
    /// for a down one.
    pub fn check_logs(&self, at: u64, images: &BTreeMap<NodeId, LogImage>) {
        let nodes: Vec<_> = images.iter().collect();
        for (i, (a_id, a)) in nodes.iter().enumerate() {
            for (b_id, b) in &nodes[i + 1..] {
                // The highest index where both logs hold the same log id.
                let matched = a
                    .entries
                    .iter()
                    .rev()
                    .find(|(index, entry)| {
                        b.entries
                            .get(index)
                            .is_some_and(|other| other.log_id == entry.log_id)
                    })
                    .map(|(index, _)| *index);
                let Some(matched) = matched else { continue };
                let differ = a.entries.range(..=matched).find(|(index, entry)| {
                    b.entries.get(index).is_some_and(|other| other != *entry)
                });
                if let Some((index, _)) = differ {
                    self.report(format!(
                        "log matching at {at}us: nodes {a_id} and {b_id} share log id {} but differ at index {index}",
                        a.entries[&matched].log_id
                    ));
                }
            }
        }
    }

    /// The checks of a finished run.
    pub fn check_history(&self, history: &[ClientOp], replicas: &BTreeMap<NodeId, Contents>) {
        let mut state = self.lock();
        let applied = state.applied.clone();

        // The value written by each applied single-put entry, and its index.
        let mut written: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        let mut expected = Contents::new();
        for (index, entry) in &applied {
            if let OrPayload::Normal(command) = &entry.payload {
                for op in &command.ops {
                    match op {
                        Op::Put { key, value } => {
                            written.insert(value.clone(), *index);
                            expected.insert(key.clone(), value.clone());
                        }
                        Op::Delete { key } => {
                            expected.remove(key);
                        }
                    }
                }
            }
        }

        for (node, contents) in replicas {
            if contents != &expected {
                Self::violation(
                    &mut state,
                    format!(
                        "node {node}'s contents differ from replaying the applied entries in order"
                    ),
                );
            }
        }

        let by_value: BTreeMap<&[u8], &ClientOp> = history
            .iter()
            .filter_map(|op| op.value.as_deref().map(|value| (value, op)))
            .collect();
        for op in history {
            match (&op.outcome, &op.value) {
                (Outcome::Acked(index), Some(value)) => {
                    let matches = applied.get(index).is_some_and(|entry| match &entry.payload {
                        OrPayload::Normal(command) => command.ops.iter().any(
                            |o| matches!(o, Op::Put { key, value: v } if key == &op.key && v == value),
                        ),
                        _ => false,
                    });
                    if !matches {
                        Self::violation(
                            &mut state,
                            format!(
                                "acknowledged write {op:?} is not the entry applied at index {index}"
                            ),
                        );
                    }
                }
                (Outcome::Read(read), None) => {
                    let read_index = match read {
                        None => None,
                        Some(value) => match written.get(value) {
                            Some(index) => Some(*index),
                            None => {
                                Self::violation(
                                    &mut state,
                                    format!("read {op:?} returned a value that never committed"),
                                );
                                continue;
                            }
                        },
                    };
                    if let Some(writer) = read.as_deref().and_then(|value| by_value.get(value))
                        && writer.start > op.end
                    {
                        Self::violation(
                            &mut state,
                            format!(
                                "read {op:?} returned a value written after it ended: {writer:?}"
                            ),
                        );
                    }
                    let stale = history.iter().find(|w| {
                        w.key == op.key
                            && w.end < op.start
                            && matches!(w.outcome, Outcome::Acked(index) if read_index.is_none_or(|r| r < index))
                    });
                    if let Some(w) = stale {
                        Self::violation(
                            &mut state,
                            format!(
                                "stale read {op:?}: write {w:?} was acknowledged before it started"
                            ),
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

impl Observer for Checker {
    fn vote_saved(&self, node: NodeId, vote: OrVote, live: &LogImage) {
        if !vote.committed {
            return;
        }
        let term = vote.leader_id.term();
        let leader = *vote.leader_id.node_id();
        let mut state = self.lock();
        match state.leaders.get(&term) {
            Some(known) if *known != leader => {
                let message = format!(
                    "election safety: term {term} has leaders {known} and {leader} (seen by {node})"
                );
                Self::violation(&mut state, message);
            }
            Some(_) => {}
            None => {
                state.leaders.insert(term, leader);
            }
        }
        if leader != node {
            return;
        }
        state.elections += 1;
        let purged = live.purged.map_or(0, |purged| purged.index);
        let missing = state
            .applied
            .iter()
            .filter(|(index, _)| **index > purged)
            .find(|(index, entry)| live.entries.get(index) != Some(*entry))
            .map(|(index, entry)| (*index, entry.log_id));
        if let Some((index, log_id)) = missing {
            let message = format!(
                "leader completeness: node {node} leads term {term} without applied entry {log_id} at {index}"
            );
            Self::violation(&mut state, message);
        }
    }

    fn applied(&self, node: NodeId, entry: &OrEntry) {
        let mut state = self.lock();
        let index = entry.log_id.index;
        match state.applied.get(&index) {
            Some(known) if known != entry => {
                let message = format!(
                    "state machine safety: node {node} applied {} at index {index}, but {} was applied there",
                    entry.log_id, known.log_id
                );
                Self::violation(&mut state, message);
            }
            Some(_) => {}
            None => {
                state.applied.insert(index, entry.clone());
            }
        }
    }
}

impl Probe for Checker {
    fn request(&self, from: NodeId, kind: &str, vote: &Vote) {
        let durable = self.durable(from).and_then(|image| image.vote);
        if !covers(durable, (*vote).into()) {
            self.report(format!(
                "persist before send: node {from} sent {kind} with vote {vote:?} but its durable vote is {durable:?}"
            ));
        }
    }

    fn vote_reply(&self, to: NodeId, req: &VoteRequest, reply: &VoteReply) {
        if !reply.granted {
            return;
        }
        let durable = self.durable(to).and_then(|image| image.vote);
        if !covers(durable, req.vote.into()) {
            self.report(format!(
                "persist before send: node {to} granted {:?} but its durable vote is {durable:?}",
                req.vote
            ));
        }
    }

    fn append_reply(&self, to: NodeId, reply: &AppendReply) {
        let AppendReply::Accepted(Some(last)) = reply else {
            return;
        };
        let (Some(log), Some(sm)) = ({
            let state = self.lock();
            (state.logs.get(&to).cloned(), state.sms.get(&to).cloned())
        }) else {
            return;
        };
        // An installed snapshot makes its entries durable in the state
        // machine before the log's purge is durable.
        let snapshot = sm.durable().and_then(|snapshot| snapshot.meta.last_log_id);
        if snapshot.is_some_and(|snapshot| snapshot.index >= last.index) {
            return;
        }
        if !log.is_durable((*last).into()) {
            self.report(format!(
                "persist before send: node {to} accepted through {last:?} before it was durable"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    //! Each check detects its violation. The simulator runs show that
    //! correct runs pass; these show that a broken run would not.

    use openraft::entry::RaftEntry;

    use super::*;
    use crate::command::Command;
    use crate::types::log_id;

    fn put(term: u64, index: u64, key: &[u8], value: &[u8]) -> OrEntry {
        let command = Command {
            ops: vec![Op::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            }],
        };
        OrEntry::new(log_id(term, index), OrPayload::Normal(command))
    }

    fn image(entries: &[OrEntry]) -> LogImage {
        LogImage {
            entries: entries
                .iter()
                .map(|e| (e.log_id.index, e.clone()))
                .collect(),
            ..LogImage::default()
        }
    }

    fn found(checker: &Checker, what: &str) -> bool {
        checker.violations().iter().any(|v| v.starts_with(what))
    }

    #[test]
    fn two_leaders_in_one_term_violate_election_safety() {
        let checker = Checker::default();
        checker.vote_saved(1, OrVote::new_committed(3, 1), &LogImage::default());
        checker.vote_saved(2, OrVote::new_committed(3, 1), &LogImage::default());
        assert!(checker.violations().is_empty(), "agreement is fine");
        checker.vote_saved(2, OrVote::new_committed(3, 2), &LogImage::default());
        assert!(found(&checker, "election safety"));
        assert_eq!(checker.elections(), 2);
    }

    #[test]
    fn a_leader_without_an_applied_entry_violates_leader_completeness() {
        let checker = Checker::default();
        let entry = put(1, 1, b"\x01k", b"v");
        checker.applied(1, &entry);
        checker.vote_saved(
            2,
            OrVote::new_committed(2, 2),
            &image(std::slice::from_ref(&entry)),
        );
        assert!(checker.violations().is_empty());
        checker.vote_saved(3, OrVote::new_committed(3, 3), &LogImage::default());
        assert!(found(&checker, "leader completeness"));
    }

    #[test]
    fn two_entries_at_one_index_violate_state_machine_safety() {
        let checker = Checker::default();
        checker.applied(1, &put(1, 1, b"\x01k", b"a"));
        checker.applied(2, &put(1, 1, b"\x01k", b"a"));
        assert!(checker.violations().is_empty(), "a redelivery is fine");
        checker.applied(3, &put(2, 1, b"\x01k", b"b"));
        assert!(found(&checker, "state machine safety"));
    }

    #[test]
    fn logs_that_share_a_log_id_but_differ_below_it_violate_log_matching() {
        let checker = Checker::default();
        let shared = put(2, 2, b"\x01k", b"x");
        let images = BTreeMap::from([
            (1, image(&[put(1, 1, b"\x01k", b"a"), shared.clone()])),
            (2, image(&[put(1, 1, b"\x01k", b"b"), shared])),
            (
                3,
                image(&[put(1, 1, b"\x01k", b"a"), put(3, 2, b"\x01k", b"y")]),
            ),
        ]);
        checker.check_logs(0, &images);
        // Node 2 differs from both others: from node 1 below a shared log
        // id, and from node 3 at one (log id 1.1 with another payload).
        // Nodes 1 and 3 agree up to where they diverge in term.
        let violations = checker.violations();
        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(violations[0].contains("nodes 1 and 2") && violations[1].contains("nodes 2 and 3"));
    }

    fn op(value: Option<&[u8]>, start: u64, end: u64, outcome: Outcome) -> ClientOp {
        ClientOp {
            client: 0,
            node: 1,
            key: b"\x01k".to_vec(),
            value: value.map(<[u8]>::to_vec),
            start,
            end,
            outcome,
        }
    }

    fn checked(history: &[ClientOp], replica: Contents) -> Vec<String> {
        let checker = Checker::default();
        checker.applied(1, &put(1, 1, b"\x01k", b"a"));
        checker.applied(1, &put(1, 2, b"\x01k", b"b"));
        checker.check_history(history, &BTreeMap::from([(1, replica)]));
        checker.violations()
    }

    #[test]
    fn the_history_checks_find_lost_writes_and_bad_reads() {
        let good = Contents::from([(b"\x01k".to_vec(), b"b".to_vec())]);
        let a = op(Some(b"a"), 0, 10, Outcome::Acked(1));
        let b = op(Some(b"b"), 20, 30, Outcome::Acked(2));
        assert_eq!(
            checked(
                &[
                    a.clone(),
                    b.clone(),
                    op(None, 40, 50, Outcome::Read(Some(b"b".to_vec())))
                ],
                good.clone()
            ),
            Vec::<String>::new()
        );

        let lost = op(Some(b"c"), 0, 10, Outcome::Acked(2));
        assert!(checked(&[lost], good.clone())[0].starts_with("acknowledged write"));
        let stale = op(None, 40, 50, Outcome::Read(Some(b"a".to_vec())));
        assert!(checked(&[a.clone(), b.clone(), stale], good.clone())[0].starts_with("stale read"));
        let missing = op(None, 40, 50, Outcome::Read(None));
        assert!(checked(&[b.clone(), missing], good.clone())[0].starts_with("stale read"));
        let dirty = op(None, 40, 50, Outcome::Read(Some(b"z".to_vec())));
        assert!(checked(&[dirty], good.clone())[0].contains("never committed"));
        let future = op(None, 0, 15, Outcome::Read(Some(b"b".to_vec())));
        assert!(checked(&[b, future], good)[0].contains("after it ended"));
        let diverged = Contents::from([(b"\x01k".to_vec(), b"a".to_vec())]);
        assert!(checked(&[], diverged)[0].contains("differ from replaying"));
    }
}
