//! Peer messages and the `Transport` trait (plan 0028, "The transport
//! trait").
//!
//! `server` carries Raft messages between processes (plan 0025, S7)
//! without depending on openraft. This module defines plain message types
//! that mirror openraft's, the trait that a transport implements over them,
//! and the conversions to and from openraft's types. No other module
//! converts peer messages.
//!
//! With standard leader ids a committed leader is identified by its term
//! alone, so a [`LogId`] is `(term, index)`, and a [`Vote`] is a term, the
//! node voted for, and whether a quorum granted it.
//!
//! A [`EntryPayload::Command`] carries the [`Command`] codec's bytes, which
//! are the bytes the Raft log stores. In S7, `peppydb.raft.v1` mirrors the
//! envelope field by field and carries these bytes opaquely.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;

use openraft::AsyncRuntime;
use openraft::entry::RaftEntry;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::AppendError;
use openraft::raft::ConflictHint;
use openraft::raft::TransferLeaderError;
use openraft::raft::TransferLeaderRequest;
use openraft::raft::TransferLeaderResponse;
use openraft::raft::VoteRequest as OrVoteRequest;
use openraft::raft::VoteResponse as OrVoteResponse;
use openraft::vote::RaftLeaderId;

use crate::command::CodecError;
use crate::command::Command;
use crate::types::Membership;
use crate::types::NodeId;
use crate::types::OrEntry;
use crate::types::OrLogId;
use crate::types::OrPayload;
use crate::types::OrSnapshotMeta;
use crate::types::OrStoredMembership;
use crate::types::OrVote;
use crate::types::PeerInfo;
use crate::types::Types;

/// A log position: the term of the leader that created the entry, and its
/// index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogId {
    pub term: u64,
    pub index: u64,
}

/// A vote: the term, the node voted for, and whether a quorum granted it
/// (then `node` is the term's leader).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Vote {
    pub term: u64,
    pub node: NodeId,
    pub committed: bool,
}

/// A membership configuration: one voter set, or two during a joint
/// change, and every member's information, learners included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipConfig {
    pub configs: Vec<BTreeSet<NodeId>>,
    pub nodes: BTreeMap<NodeId, PeerInfo>,
}

/// What a log entry carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryPayload {
    /// A new leader's no-op.
    Blank,
    /// The [`Command`] codec's bytes, opaque to the transport.
    Command(Vec<u8>),
    Membership(MembershipConfig),
}

/// One log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub log_id: LogId,
    pub payload: EntryPayload,
}

/// Replicate entries, or send a heartbeat when `entries` is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendEntries {
    pub vote: Vote,
    pub prev: Option<LogId>,
    pub entries: Vec<Entry>,
    pub commit: Option<LogId>,
}

/// A follower's answer to [`AppendEntries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendReply {
    /// Accepted; the last log id the follower holds from this request.
    Accepted(Option<LogId>),
    /// The follower does not hold `log_id`, the request's `prev`. `hint`
    /// lists log ids that the follower holds, to guide the next probe.
    Conflict {
        log_id: LogId,
        hint: Option<Vec<LogId>>,
    },
    /// The follower has seen a higher vote.
    HigherVote(Vote),
}

/// Ask for a vote, or, as a pre-vote, ask whether a vote would be granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoteRequest {
    pub vote: Vote,
    pub last: Option<LogId>,
    /// Set when the old leader hands leadership to this candidate.
    pub transfer_from: Option<Vote>,
}

/// The answer to a [`VoteRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoteReply {
    pub vote: Vote,
    pub granted: bool,
    pub last: Option<LogId>,
}

/// What a snapshot holds: the last log id it includes and the membership
/// at that point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMeta {
    pub last: Option<LogId>,
    pub membership_log_id: Option<LogId>,
    pub membership: MembershipConfig,
}

/// The leader asks `to` to take over leadership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferLeader {
    pub from: Vote,
    pub to: NodeId,
    pub last: Option<LogId>,
}

/// The answer to [`TransferLeader`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferReply {
    Accepted,
    VoteChanged {
        expected: Vote,
        actual: Vote,
    },
    LogNotFlushed {
        expected: Option<LogId>,
        actual: Option<LogId>,
    },
}

/// A message could not be delivered or answered. openraft retries later.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("peer unreachable: {0}")]
pub struct Unreachable(pub String);

/// What `server_raft` needs from the network. `from` is the sending node,
/// so a transport can model partitions. S7 implements this in `server`
/// over gRPC; tests use the simulator's network.
pub trait Transport: Send + Sync + 'static {
    /// The snapshot data that this transport carries; the state machine's.
    type SnapshotData: Send + 'static;

    fn append_entries(
        &self,
        from: NodeId,
        to: NodeId,
        req: AppendEntries,
    ) -> impl Future<Output = Result<AppendReply, Unreachable>> + Send;

    fn vote(
        &self,
        from: NodeId,
        to: NodeId,
        req: VoteRequest,
    ) -> impl Future<Output = Result<VoteReply, Unreachable>> + Send;

    fn pre_vote(
        &self,
        from: NodeId,
        to: NodeId,
        req: VoteRequest,
    ) -> impl Future<Output = Result<VoteReply, Unreachable>> + Send;

    /// Send a whole snapshot; the reply is the receiver's vote.
    fn snapshot(
        &self,
        from: NodeId,
        to: NodeId,
        vote: Vote,
        meta: SnapshotMeta,
        data: Self::SnapshotData,
    ) -> impl Future<Output = Result<Vote, Unreachable>> + Send;

    fn transfer_leader(
        &self,
        from: NodeId,
        to: NodeId,
        req: TransferLeader,
    ) -> impl Future<Output = Result<TransferReply, Unreachable>> + Send;
}

// Conversions. Each pair round-trips exactly; the tests check it.

impl From<OrLogId> for LogId {
    fn from(log_id: OrLogId) -> Self {
        LogId {
            term: log_id.leader_id.term,
            index: log_id.index,
        }
    }
}

impl From<LogId> for OrLogId {
    fn from(log_id: LogId) -> Self {
        OrLogId::new_term_index(log_id.term, log_id.index)
    }
}

impl From<OrVote> for Vote {
    fn from(vote: OrVote) -> Self {
        Vote {
            term: vote.leader_id.term(),
            node: *vote.leader_id.node_id(),
            committed: vote.committed,
        }
    }
}

impl From<Vote> for OrVote {
    fn from(vote: Vote) -> Self {
        match vote.committed {
            true => OrVote::new_committed(vote.term, vote.node),
            false => OrVote::new(vote.term, vote.node),
        }
    }
}

impl From<&Membership> for MembershipConfig {
    fn from(membership: &Membership) -> Self {
        MembershipConfig {
            configs: membership.get_joint_config().clone(),
            nodes: membership
                .nodes()
                .map(|(id, node)| (*id, node.clone()))
                .collect(),
        }
    }
}

/// Why a received message does not convert to openraft's types.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
    #[error("bad command in an entry: {0}")]
    Command(#[from] CodecError),
    #[error("bad membership: {0}")]
    Membership(String),
}

impl TryFrom<MembershipConfig> for Membership {
    type Error = MessageError;

    fn try_from(config: MembershipConfig) -> Result<Self, MessageError> {
        Membership::new(config.configs, config.nodes)
            .map_err(|error| MessageError::Membership(error.to_string()))
    }
}

fn opt<A: Into<B>, B>(value: Option<A>) -> Option<B> {
    value.map(Into::into)
}

impl TryFrom<&OrEntry> for Entry {
    type Error = CodecError;

    fn try_from(entry: &OrEntry) -> Result<Self, CodecError> {
        let payload = match &entry.payload {
            OrPayload::Blank => EntryPayload::Blank,
            OrPayload::Normal(command) => EntryPayload::Command(command.to_bytes()?),
            OrPayload::Membership(membership) => EntryPayload::Membership(membership.into()),
        };
        Ok(Entry {
            log_id: entry.log_id.into(),
            payload,
        })
    }
}

impl TryFrom<Entry> for OrEntry {
    type Error = MessageError;

    fn try_from(entry: Entry) -> Result<Self, MessageError> {
        let payload = match entry.payload {
            EntryPayload::Blank => OrPayload::Blank,
            EntryPayload::Command(bytes) => OrPayload::Normal(Command::decode(&bytes)?),
            EntryPayload::Membership(config) => OrPayload::Membership(config.try_into()?),
        };
        Ok(OrEntry::new(entry.log_id.into(), payload))
    }
}

impl<RT: AsyncRuntime> TryFrom<AppendEntriesRequest<Types<RT>>> for AppendEntries {
    type Error = CodecError;

    fn try_from(req: AppendEntriesRequest<Types<RT>>) -> Result<Self, CodecError> {
        Ok(AppendEntries {
            vote: req.vote.into(),
            prev: opt(req.prev_log_id),
            entries: req
                .entries
                .iter()
                .map(Entry::try_from)
                .collect::<Result<_, _>>()?,
            commit: opt(req.leader_commit),
        })
    }
}

impl<RT: AsyncRuntime> TryFrom<AppendEntries> for AppendEntriesRequest<Types<RT>> {
    type Error = MessageError;

    fn try_from(req: AppendEntries) -> Result<Self, MessageError> {
        Ok(AppendEntriesRequest {
            vote: req.vote.into(),
            prev_log_id: opt(req.prev),
            entries: req
                .entries
                .into_iter()
                .map(OrEntry::try_from)
                .collect::<Result<_, _>>()?,
            leader_commit: opt(req.commit),
        })
    }
}

/// openraft's answer to an append request.
type OrAppendResult<RT> = Result<Option<OrLogId>, AppendError<Types<RT>>>;

impl<RT: AsyncRuntime> From<OrAppendResult<RT>> for AppendReply {
    fn from(result: OrAppendResult<RT>) -> Self {
        match result {
            Ok(last) => AppendReply::Accepted(opt(last)),
            Err(AppendError::Conflict { log_id, hint }) => AppendReply::Conflict {
                log_id: log_id.into(),
                hint: hint.map(|hint| hint.log_ids.into_iter().map(Into::into).collect()),
            },
            Err(AppendError::HigherVote(vote)) => AppendReply::HigherVote(vote.into()),
        }
    }
}

impl<RT: AsyncRuntime> From<AppendReply> for OrAppendResult<RT> {
    fn from(reply: AppendReply) -> Self {
        match reply {
            AppendReply::Accepted(last) => Ok(opt(last)),
            AppendReply::Conflict { log_id, hint } => Err(AppendError::Conflict {
                log_id: log_id.into(),
                hint: hint.map(|ids| ConflictHint::new(ids.into_iter().map(Into::into).collect())),
            }),
            AppendReply::HigherVote(vote) => Err(AppendError::HigherVote(vote.into())),
        }
    }
}

impl<RT: AsyncRuntime> From<OrVoteRequest<Types<RT>>> for VoteRequest {
    fn from(req: OrVoteRequest<Types<RT>>) -> Self {
        VoteRequest {
            vote: req.vote.into(),
            last: opt(req.last_log_id),
            transfer_from: opt(req.leadership_transfer_from),
        }
    }
}

impl<RT: AsyncRuntime> From<VoteRequest> for OrVoteRequest<Types<RT>> {
    fn from(req: VoteRequest) -> Self {
        let mut or = OrVoteRequest::new(req.vote.into(), opt(req.last));
        or.leadership_transfer_from = opt(req.transfer_from);
        or
    }
}

impl<RT: AsyncRuntime> From<OrVoteResponse<Types<RT>>> for VoteReply {
    fn from(reply: OrVoteResponse<Types<RT>>) -> Self {
        VoteReply {
            vote: reply.vote.into(),
            granted: reply.vote_granted,
            last: opt(reply.last_log_id),
        }
    }
}

impl<RT: AsyncRuntime> From<VoteReply> for OrVoteResponse<Types<RT>> {
    fn from(reply: VoteReply) -> Self {
        OrVoteResponse::new(OrVote::from(reply.vote), opt(reply.last), reply.granted)
    }
}

impl From<&OrSnapshotMeta> for SnapshotMeta {
    fn from(meta: &OrSnapshotMeta) -> Self {
        SnapshotMeta {
            last: opt(meta.last_log_id),
            membership_log_id: opt(*meta.last_membership.log_id()),
            membership: meta.last_membership.membership().into(),
        }
    }
}

impl TryFrom<SnapshotMeta> for OrSnapshotMeta {
    type Error = MessageError;

    fn try_from(meta: SnapshotMeta) -> Result<Self, MessageError> {
        Ok(OrSnapshotMeta {
            last_log_id: opt(meta.last),
            last_membership: OrStoredMembership::new(
                opt(meta.membership_log_id),
                meta.membership.try_into()?,
            ),
        })
    }
}

impl<RT: AsyncRuntime> From<&TransferLeaderRequest<Types<RT>>> for TransferLeader {
    fn from(req: &TransferLeaderRequest<Types<RT>>) -> Self {
        TransferLeader {
            from: (*req.from_leader()).into(),
            to: *req.to_node_id(),
            last: req.last_log_id().map(|log_id| (*log_id).into()),
        }
    }
}

impl<RT: AsyncRuntime> From<TransferLeader> for TransferLeaderRequest<Types<RT>> {
    fn from(req: TransferLeader) -> Self {
        TransferLeaderRequest::new(req.from.into(), req.to, opt(req.last))
    }
}

impl<RT: AsyncRuntime> From<TransferLeaderResponse<Types<RT>>> for TransferReply {
    fn from(reply: TransferLeaderResponse<Types<RT>>) -> Self {
        match reply {
            Ok(()) => TransferReply::Accepted,
            Err(TransferLeaderError::VoteChanged { expected, actual }) => {
                TransferReply::VoteChanged {
                    expected: expected.into(),
                    actual: actual.into(),
                }
            }
            Err(TransferLeaderError::LogNotFlushed { expected, actual }) => {
                TransferReply::LogNotFlushed {
                    expected: opt(expected),
                    actual: opt(actual),
                }
            }
        }
    }
}

impl<RT: AsyncRuntime> From<TransferReply> for TransferLeaderResponse<Types<RT>> {
    fn from(reply: TransferReply) -> Self {
        match reply {
            TransferReply::Accepted => Ok(()),
            TransferReply::VoteChanged { expected, actual } => {
                Err(TransferLeaderError::VoteChanged {
                    expected: expected.into(),
                    actual: actual.into(),
                })
            }
            TransferReply::LogNotFlushed { expected, actual } => {
                Err(TransferLeaderError::LogNotFlushed {
                    expected: opt(expected),
                    actual: opt(actual),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use openraft_rt_sim::SimRuntime;

    use super::*;
    use crate::command::Op;
    use crate::types::log_id;

    type C = Types<SimRuntime>;

    fn membership() -> Membership {
        let nodes: BTreeMap<u64, PeerInfo> = [1, 2, 3]
            .into_iter()
            .map(|id| {
                (
                    id,
                    PeerInfo {
                        addr: format!("n{id}"),
                    },
                )
            })
            .collect();
        Membership::new(vec![[1, 2].into(), [1, 2, 3].into()], nodes).unwrap()
    }

    fn entries() -> Vec<OrEntry> {
        let command = Command {
            ops: vec![Op::Put {
                key: b"\x01k".to_vec(),
                value: b"v".to_vec(),
            }],
        };
        vec![
            OrEntry::new(log_id(2, 3), OrPayload::Blank),
            OrEntry::new(log_id(2, 4), OrPayload::Normal(command)),
            OrEntry::new(log_id(2, 5), OrPayload::Membership(membership())),
        ]
    }

    #[test]
    fn every_message_round_trips() {
        let request = AppendEntriesRequest::<C> {
            vote: OrVote::new_committed(2, 1),
            prev_log_id: Some(log_id(1, 2)),
            entries: entries(),
            leader_commit: Some(log_id(1, 2)),
        };
        let ours = AppendEntries::try_from(request.clone()).unwrap();
        let back: AppendEntriesRequest<C> = ours.try_into().unwrap();
        assert_eq!(back.vote, request.vote);
        assert_eq!(back.prev_log_id, request.prev_log_id);
        assert_eq!(back.entries, request.entries);
        assert_eq!(back.leader_commit, request.leader_commit);

        let replies: Vec<Result<Option<OrLogId>, AppendError<C>>> = vec![
            Ok(None),
            Ok(Some(log_id(2, 5))),
            Err(AppendError::Conflict {
                log_id: log_id(1, 2),
                hint: Some(ConflictHint::new(vec![log_id(1, 1)])),
            }),
            Err(AppendError::Conflict {
                log_id: log_id(1, 2),
                hint: None,
            }),
            Err(AppendError::HigherVote(OrVote::new(3, 2))),
        ];
        for reply in replies {
            let back: Result<Option<OrLogId>, AppendError<C>> =
                AppendReply::from(reply.clone()).into();
            assert_eq!(back, reply);
        }

        let mut vote = OrVoteRequest::<C>::new(OrVote::new(3, 2), Some(log_id(2, 5)));
        vote.leadership_transfer_from = Some(OrVote::new_committed(2, 1));
        let back: OrVoteRequest<C> = VoteRequest::from(vote.clone()).into();
        assert_eq!(back, vote);
        let reply = OrVoteResponse::<C>::new(OrVote::new(3, 2), Some(log_id(2, 5)), true);
        let back: OrVoteResponse<C> = VoteReply::from(reply.clone()).into();
        assert_eq!(back, reply);

        let meta = OrSnapshotMeta {
            last_log_id: Some(log_id(2, 5)),
            last_membership: OrStoredMembership::new(Some(log_id(2, 5)), membership()),
        };
        let back: OrSnapshotMeta = SnapshotMeta::from(&meta).try_into().unwrap();
        assert_eq!(back, meta);

        let transfer =
            TransferLeaderRequest::<C>::new(OrVote::new_committed(2, 1), 3, Some(log_id(2, 5)));
        let back: TransferLeaderRequest<C> = TransferLeader::from(&transfer).into();
        assert_eq!(back, transfer);
        let rejected: TransferLeaderResponse<C> = Err(TransferLeaderError::LogNotFlushed {
            expected: Some(log_id(2, 5)),
            actual: None,
        });
        let back: TransferLeaderResponse<C> = TransferReply::from(rejected.clone()).into();
        assert_eq!(back, rejected);
    }

    #[test]
    fn commands_travel_as_codec_bytes() {
        let entry = &entries()[1];
        let ours = Entry::try_from(entry).unwrap();
        let EntryPayload::Command(bytes) = &ours.payload else {
            panic!("not a command: {ours:?}");
        };
        let OrPayload::Normal(command) = &entry.payload else {
            unreachable!()
        };
        assert_eq!(bytes, &command.to_bytes().unwrap());

        let bad = Entry {
            log_id: LogId { term: 1, index: 1 },
            payload: EntryPayload::Command(vec![9]),
        };
        assert!(matches!(
            OrEntry::try_from(bad),
            Err(MessageError::Command(_))
        ));
    }
}
