//! The `RaftNode` facade (plan 0028, "The facade").
//!
//! `server` proposes writes, asks for read barriers, reads status, and
//! hands incoming peer messages to the node, all in peppydb types. Only
//! this module and [`crate::error`] call openraft's `Raft` handle.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use openraft::AsyncRuntime;
use openraft::Config;
use openraft::ReadPolicy;
use openraft::ServerState;
use openraft::SnapshotPolicy;
use openraft::async_runtime::WatchReceiver;
use openraft::error::ClientWriteError;
use openraft::error::RaftError;
use openraft::storage::RaftLogStorage;
use openraft::storage::RaftStateMachine;
use openraft::storage::Snapshot;

use crate::command::Command;
use crate::error::ProposeError;
use crate::error::ReadError;
use crate::error::StartError;
use crate::error::StopError;
use crate::error::leader_hint;
use crate::network::Factory;
use crate::transport::AppendEntries;
use crate::transport::AppendReply;
use crate::transport::LogId;
use crate::transport::MessageError;
use crate::transport::SnapshotMeta;
use crate::transport::TransferLeader;
use crate::transport::TransferReply;
use crate::transport::Transport;
use crate::transport::Vote;
use crate::transport::VoteReply;
use crate::transport::VoteRequest;
use crate::types::NodeId;
use crate::types::PeerInfo;
use crate::types::Types;

/// Timing and compaction settings. The defaults suit a LAN; the simulator
/// uses the same values on its virtual clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaftConfig {
    /// The election timeout is drawn from this range for each round.
    pub election_timeout: (Duration, Duration),
    pub heartbeat_interval: Duration,
    /// Build a snapshot after this many entries since the last one; `None`
    /// builds them only on demand.
    pub snapshot_after: Option<u64>,
    /// Entries kept in the log below the last snapshot, so a follower that
    /// is slightly behind still gets entries rather than a snapshot.
    pub keep_below_snapshot: u64,
    /// Pre-vote (dissertation §9.6): a node asks whether it would win
    /// before it raises its term.
    pub pre_vote: bool,
}

impl Default for RaftConfig {
    fn default() -> Self {
        Self {
            election_timeout: (Duration::from_millis(150), Duration::from_millis(300)),
            heartbeat_interval: Duration::from_millis(50),
            snapshot_after: Some(5000),
            keep_below_snapshot: 1000,
            pre_vote: true,
        }
    }
}

impl RaftConfig {
    fn to_openraft(&self) -> Result<Config, StartError> {
        let millis = |d: Duration| d.as_millis() as u64;
        let config = Config {
            cluster_name: "peppydb".into(),
            election_timeout_min: millis(self.election_timeout.0),
            election_timeout_max: millis(self.election_timeout.1),
            heartbeat_interval: millis(self.heartbeat_interval),
            snapshot_policy: match self.snapshot_after {
                Some(n) => SnapshotPolicy::LogsSinceLast(n),
                None => SnapshotPolicy::Never,
            },
            max_in_snapshot_log_to_keep: self.keep_below_snapshot,
            enable_pre_vote: Some(self.pre_vote),
            ..Config::default()
        };
        config
            .validate()
            .map_err(|error| StartError::Config(error.to_string()))
    }
}

/// A write that committed and was applied on this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proposed {
    pub log_id: LogId,
}

/// The point a linearizable read must observe: this node has applied
/// through it when [`RaftNode::read_barrier`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadPoint {
    pub index: u64,
}

/// A node's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Leader,
    Candidate,
    Follower,
    Learner,
    Stopped,
}

/// A snapshot of a node's Raft state, for `Admin.Status` and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub id: NodeId,
    pub role: Role,
    pub term: u64,
    pub vote: Vote,
    pub leader: Option<NodeId>,
    pub last_log_index: Option<u64>,
    pub committed: Option<LogId>,
    pub applied: Option<LogId>,
    pub snapshot: Option<LogId>,
    pub purged: Option<LogId>,
    /// Set when the node stopped because of an error.
    pub failure: Option<String>,
}

/// Why an incoming peer message was not handled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandleError {
    #[error(transparent)]
    BadMessage(#[from] MessageError),
    #[error("the node stopped: {0}")]
    Stopped(String),
}

/// One Raft node: openraft behind peppydb's types. Clones share the node.
pub struct RaftNode<SM, RT = openraft_rt_tokio::TokioRuntime>
where
    RT: AsyncRuntime,
    SM: RaftStateMachine<Types<RT>>,
{
    id: NodeId,
    raft: openraft::Raft<Types<RT>, SM>,
}

impl<SM, RT> Clone for RaftNode<SM, RT>
where
    RT: AsyncRuntime,
    SM: RaftStateMachine<Types<RT>>,
{
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            raft: self.raft.clone(),
        }
    }
}

fn opt<A: Into<B>, B>(value: Option<A>) -> Option<B> {
    value.map(Into::into)
}

impl<SM, RT> RaftNode<SM, RT>
where
    RT: AsyncRuntime,
    SM: RaftStateMachine<Types<RT>>,
{
    /// Start a node over its log, state machine, and transport. It recovers
    /// its state from storage and starts its timers.
    pub async fn start<LS, T>(
        id: NodeId,
        config: &RaftConfig,
        log: LS,
        sm: SM,
        transport: Arc<T>,
    ) -> Result<Self, StartError>
    where
        LS: RaftLogStorage<Types<RT>>,
        T: Transport<SnapshotData = SM::SnapshotData>,
    {
        let config = Arc::new(config.to_openraft()?);
        let network = Factory { id, transport };
        let raft = openraft::Raft::new(id, config, network, log, sm)
            .await
            .map_err(|error| StartError::Storage(error.to_string()))?;
        Ok(Self { id, raft })
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Form a new cluster with these voters. Call it on one node of a new
    /// cluster; a node that already holds state refuses.
    pub async fn initialize(&self, voters: BTreeMap<NodeId, PeerInfo>) -> Result<(), StartError> {
        self.raft
            .initialize(voters)
            .await
            .map_err(|error| StartError::Initialize(error.to_string()))
    }

    /// Propose a write. Returns once it is committed and applied on this
    /// node.
    pub async fn propose(&self, command: Command) -> Result<Proposed, ProposeError> {
        match self.raft.client_write(command).await {
            Ok(response) => Ok(Proposed {
                log_id: response.log_id.into(),
            }),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => {
                Err(ProposeError::NotLeader(leader_hint(&forward)))
            }
            Err(RaftError::APIError(ClientWriteError::LogEntryDiscarded(_))) => {
                Err(ProposeError::OutcomeUnknown)
            }
            Err(RaftError::APIError(
                error @ (ClientWriteError::ChangeMembershipError(_)
                | ClientWriteError::PreconditionFailed(_)),
            )) => Err(ProposeError::Internal(error.to_string())),
            Err(RaftError::Fatal(fatal)) => Err(ProposeError::Stopped(fatal.to_string())),
        }
    }

    /// Read index (dissertation §6.4): confirm leadership with a quorum and
    /// wait until this node has applied through the read point. A read of
    /// local state after this returns is linearizable.
    pub async fn read_barrier(&self) -> Result<ReadPoint, ReadError> {
        match self.raft.ensure_linearizable(ReadPolicy::ReadIndex).await {
            Ok(read) => Ok(ReadPoint {
                index: read.index(),
            }),
            Err(error) => Err(crate::error::read_error(error)),
        }
    }

    /// Ask the leader to hand leadership to `to`. Returns at once; Raft
    /// runs the handover.
    pub async fn transfer_leader(&self, to: NodeId) -> Result<(), StopError> {
        self.raft
            .trigger()
            .transfer_leader(to)
            .await
            .map_err(|fatal| StopError(fatal.to_string()))
    }

    /// Build a snapshot now, which also lets the log be purged below it.
    pub async fn snapshot(&self) -> Result<(), StopError> {
        self.raft
            .trigger()
            .snapshot()
            .await
            .map_err(|fatal| StopError(fatal.to_string()))
    }

    /// The node's current Raft state.
    pub fn status(&self) -> Status {
        let metrics = self.raft.metrics().borrow_watched().clone();
        let role = match metrics.state {
            ServerState::Leader => Role::Leader,
            ServerState::Candidate => Role::Candidate,
            ServerState::Follower => Role::Follower,
            ServerState::Learner => Role::Learner,
            ServerState::Shutdown => Role::Stopped,
        };
        Status {
            id: self.id,
            role,
            term: metrics.current_term,
            vote: metrics.vote.into(),
            leader: metrics.current_leader,
            last_log_index: metrics.last_log_index,
            committed: opt(metrics.local_committed),
            applied: opt(metrics.last_applied),
            snapshot: opt(metrics.snapshot),
            purged: opt(metrics.purged),
            failure: metrics.running_state.err().map(|fatal| fatal.to_string()),
        }
    }

    /// Stop the node's tasks.
    pub async fn shutdown(&self) {
        // A join error means a task already ended, which is the goal.
        let _ = self.raft.shutdown().await;
    }

    // Incoming peer messages. The transport calls these on the receiver.

    pub async fn handle_append_entries(
        &self,
        req: AppendEntries,
    ) -> Result<AppendReply, HandleError> {
        let req = req.try_into()?;
        let result = self.raft.append_entries(req).await.map_err(stopped)?;
        Ok(AppendReply::from(result))
    }

    pub async fn handle_vote(&self, req: VoteRequest) -> Result<VoteReply, HandleError> {
        let reply = self.raft.vote(req.into()).await.map_err(stopped)?;
        Ok(VoteReply::from(reply))
    }

    pub async fn handle_pre_vote(&self, req: VoteRequest) -> Result<VoteReply, HandleError> {
        let reply = self.raft.pre_vote(req.into()).await.map_err(stopped)?;
        Ok(VoteReply::from(reply))
    }

    pub async fn handle_snapshot(
        &self,
        vote: Vote,
        meta: SnapshotMeta,
        data: SM::SnapshotData,
    ) -> Result<Vote, HandleError> {
        let snapshot = Snapshot {
            meta: meta.try_into()?,
            snapshot: data,
        };
        let reply = self
            .raft
            .install_full_snapshot(vote.into(), snapshot)
            .await
            .map_err(|fatal| HandleError::Stopped(fatal.to_string()))?;
        Ok(reply.vote.into())
    }

    pub async fn handle_transfer_leader(
        &self,
        req: TransferLeader,
    ) -> Result<TransferReply, HandleError> {
        let reply = self
            .raft
            .handle_transfer_leader(req.into())
            .await
            .map_err(|fatal| HandleError::Stopped(fatal.to_string()))?;
        Ok(TransferReply::from(reply))
    }
}

fn stopped<E: std::fmt::Display>(error: E) -> HandleError {
    HandleError::Stopped(error.to_string())
}
