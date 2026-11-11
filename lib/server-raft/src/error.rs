//! The facade's errors, and the only matches on openraft's error enums
//! outside the facade's own calls.

use openraft::AsyncRuntime;
use openraft::error::ForwardToLeader;
use openraft::error::LinearizableReadError;
use openraft::error::RaftError;

use crate::types::NodeId;
use crate::types::PeerInfo;
use crate::types::Types;

/// Where the leader is, if this node knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderHint {
    pub id: Option<NodeId>,
    pub node: Option<PeerInfo>,
}

pub(crate) fn leader_hint<RT: AsyncRuntime>(forward: &ForwardToLeader<Types<RT>>) -> LeaderHint {
    LeaderHint {
        id: forward.leader_id,
        node: forward.leader_node.clone(),
    }
}

/// Why a proposal did not return [`crate::Proposed`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProposeError {
    /// This node is not the leader. Nothing was proposed; retrying on the
    /// leader is safe.
    #[error("not the leader (leader: {:?})", .0.id)]
    NotLeader(LeaderHint),
    /// The entry was appended, then discarded from this node's log on a
    /// leader change. It may still commit through another leader: the
    /// outcome is unknown (plan 0026's rule for writes).
    #[error("leadership changed after the write was proposed; its outcome is unknown")]
    OutcomeUnknown,
    /// The node stopped, for example after a storage error. It must be
    /// restarted (fail-stop, as in plan 0026).
    #[error("the node stopped: {0}")]
    Stopped(String),
    /// A server bug, such as a membership change through `propose`.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Why a read barrier failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    #[error("not the leader (leader: {:?})", .0.id)]
    NotLeader(LeaderHint),
    /// The leader could not confirm its leadership with a quorum.
    #[error("no quorum confirmed leadership")]
    NoQuorum,
    #[error("the node stopped: {0}")]
    Stopped(String),
}

pub(crate) fn read_error<RT: AsyncRuntime>(
    error: RaftError<Types<RT>, LinearizableReadError<Types<RT>>>,
) -> ReadError {
    match error {
        RaftError::APIError(LinearizableReadError::ForwardToLeader(forward)) => {
            ReadError::NotLeader(leader_hint(&forward))
        }
        RaftError::APIError(LinearizableReadError::QuorumNotEnough(_)) => ReadError::NoQuorum,
        RaftError::Fatal(fatal) => ReadError::Stopped(fatal.to_string()),
    }
}

/// Why a node did not start or initialize.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("storage failed during recovery: {0}")]
    Storage(String),
    #[error("cannot initialize: {0}")]
    Initialize(String),
}

/// The node stopped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the node stopped: {0}")]
pub struct StopError(pub String);
