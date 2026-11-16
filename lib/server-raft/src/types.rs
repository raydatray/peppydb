//! The openraft type configuration (plan 0028, "Type config").
//!
//! openraft is generic over one marker type that names every type it uses:
//! the payload, node identity, leader identity, log entries, and the async
//! runtime. [`Types`] is that marker. It is generic over the runtime, so the
//! same storage and state-machine code runs on Tokio in production and on
//! the deterministic simulator in tests.

use std::cmp::Ordering;
use std::fmt;
use std::marker::PhantomData;

use openraft::AsyncRuntime;
use openraft::OptionalSend;
use openraft::RaftTypeConfig;
use openraft::vote::RaftLeaderId;

use crate::command::Command;

/// What membership records for each node: the address the peer transport
/// dials (plan 0025, S7). Empty until the transport exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerInfo {
    pub addr: String,
}

/// A node's identity in votes, membership, and leader hints.
pub type NodeId = u64;

/// Standard Raft leader identity: at most one leader per term (paper §5.2,
/// Figure 3). openraft's default, `leader_id_adv`, allows several leaders
/// per term; the election-safety check depends on this choice.
pub type LeaderId = openraft::impls::leader_id_std::LeaderId<u64, NodeId>;

/// The committed form of [`LeaderId`]: the term alone.
pub type CommittedLeaderId = <LeaderId as RaftLeaderId>::Committed;

/// openraft's membership type over our node identity and information.
pub type Membership = openraft::impls::Membership<NodeId, PeerInfo>;

/// The marker that names every type openraft uses. `RT` is the async
/// runtime: `TokioRuntime` in production, `SimRuntime` in tests.
pub struct Types<RT>(PhantomData<fn() -> RT>);

// The derives would require `RT` itself to be `Clone`, `Ord`, and so on.
// `Types` holds no `RT`, so these impls hold for every runtime.
impl<RT> fmt::Debug for Types<RT> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Types")
    }
}

impl<RT> Clone for Types<RT> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<RT> Copy for Types<RT> {}

impl<RT> Default for Types<RT> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<RT> PartialEq for Types<RT> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl<RT> Eq for Types<RT> {}

impl<RT> PartialOrd for Types<RT> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<RT> Ord for Types<RT> {
    fn cmp(&self, _: &Self) -> Ordering {
        Ordering::Equal
    }
}

impl<RT: AsyncRuntime> RaftTypeConfig for Types<RT> {
    type D = Command;
    /// A write's response carries its log id; the state machine returns
    /// nothing else.
    type R = ();
    type NodeId = NodeId;
    type Node = PeerInfo;
    type Term = u64;
    type LeaderId = LeaderId;
    type Vote = openraft::impls::Vote<LeaderId>;
    type Membership = Membership;
    type MembershipEdit = openraft::impls::ChangeMembers<NodeId, PeerInfo>;
    type Payload = OrPayload;
    type Entry = OrEntry;
    type AsyncRuntime = RT;
    type Responder<T>
        = openraft::impls::OneshotResponder<Self, T>
    where
        T: OptionalSend + 'static;
    type Batch<T>
        = openraft::impls::InlineBatch<T>
    where
        T: OptionalSend + 'static;
    type ErrorSource = openraft::AnyError;
}

/// openraft's vote under [`Types`].
pub(crate) type OrVote = openraft::impls::Vote<LeaderId>;
/// openraft's log id under [`Types`].
pub(crate) type OrLogId = openraft::impls::LogId<CommittedLeaderId>;
/// openraft's entry payload under [`Types`].
pub(crate) type OrPayload = openraft::impls::EntryPayload<Command, Membership>;
/// openraft's log entry under [`Types`].
pub(crate) type OrEntry = openraft::impls::Entry<CommittedLeaderId, OrPayload>;

/// openraft's stored membership under [`Types`].
pub(crate) type OrStoredMembership = openraft::StoredMembership<CommittedLeaderId, Membership>;
/// openraft's snapshot metadata under [`Types`].
pub(crate) type OrSnapshotMeta = openraft::storage::SnapshotMeta<CommittedLeaderId, Membership>;
/// openraft's snapshot, with the bytes the models use as snapshot data.
#[cfg(any(test, feature = "sim"))]
pub(crate) type OrSnapshot = openraft::storage::Snapshot<CommittedLeaderId, Membership, Vec<u8>>;

/// A log id from a term and an index.
#[cfg(test)]
pub(crate) fn log_id(term: u64, index: u64) -> OrLogId {
    OrLogId::new_term_index(term, index)
}

#[cfg(test)]
mod tests {
    use openraft_rt_tokio::TokioRuntime;

    use super::*;

    fn assert_config<C: RaftTypeConfig>() {}

    #[test]
    fn the_config_holds_for_both_runtimes() {
        assert_config::<Types<TokioRuntime>>();
        assert_config::<Types<openraft_rt_sim::SimRuntime>>();
    }

    /// Standard leader ids: two candidates in one term are not ordered, so
    /// a voter that granted one cannot be moved to the other in the same
    /// term. Advanced leader ids would order them by node id.
    #[test]
    fn leader_ids_are_standard() {
        let a = OrVote::new(5, 1);
        let b = OrVote::new(5, 2);
        assert_eq!(a.partial_cmp(&b), None);
        assert_eq!(b.partial_cmp(&a), None);

        let adv = |node| openraft::impls::leader_id_adv::LeaderId::<u64, u64>::new(5, node);
        assert!(
            adv(1) < adv(2),
            "the advanced mode orders a term's candidates"
        );

        // A higher term still wins, and a committed vote of a term is above
        // any uncommitted vote of that term.
        assert!(OrVote::new(6, 1) > b);
        assert!(OrVote::new_committed(5, 1) > b);
    }
}
