//! `Router`: a perfect in-memory [`Transport`]. It delivers every message
//! at once to the target node's handler. The simulator's network adds
//! faults on top of it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use openraft::AsyncRuntime;
use openraft::storage::RaftStateMachine;

use crate::node::RaftNode;
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

/// Delivers messages to registered nodes. A node that is not registered,
/// for example one that crashed, is unreachable.
pub struct Router<SM, RT>
where
    RT: AsyncRuntime,
    SM: RaftStateMachine<Types<RT>>,
{
    nodes: Mutex<BTreeMap<NodeId, RaftNode<SM, RT>>>,
}

impl<SM, RT> Default for Router<SM, RT>
where
    RT: AsyncRuntime,
    SM: RaftStateMachine<Types<RT>>,
{
    fn default() -> Self {
        Self {
            nodes: Mutex::new(BTreeMap::new()),
        }
    }
}

impl<SM, RT> Router<SM, RT>
where
    RT: AsyncRuntime,
    SM: RaftStateMachine<Types<RT>>,
{
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn register(&self, node: RaftNode<SM, RT>) {
        self.nodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(node.id(), node);
    }

    pub fn unregister(&self, id: NodeId) -> Option<RaftNode<SM, RT>> {
        self.nodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
    }

    pub fn node(&self, id: NodeId) -> Result<RaftNode<SM, RT>, Unreachable> {
        let nodes = self.nodes.lock().unwrap_or_else(PoisonError::into_inner);
        nodes
            .get(&id)
            .cloned()
            .ok_or_else(|| Unreachable(format!("node {id} is down")))
    }
}

fn delivered<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, Unreachable> {
    result.map_err(|error| Unreachable(error.to_string()))
}

impl<SM, RT> Transport for Router<SM, RT>
where
    RT: AsyncRuntime,
    SM: RaftStateMachine<Types<RT>>,
    SM::SnapshotData: Send,
{
    type SnapshotData = SM::SnapshotData;

    async fn append_entries(
        &self,
        _from: NodeId,
        to: NodeId,
        req: AppendEntries,
    ) -> Result<AppendReply, Unreachable> {
        delivered(self.node(to)?.handle_append_entries(req).await)
    }

    async fn vote(
        &self,
        _from: NodeId,
        to: NodeId,
        req: VoteRequest,
    ) -> Result<VoteReply, Unreachable> {
        delivered(self.node(to)?.handle_vote(req).await)
    }

    async fn pre_vote(
        &self,
        _from: NodeId,
        to: NodeId,
        req: VoteRequest,
    ) -> Result<VoteReply, Unreachable> {
        delivered(self.node(to)?.handle_pre_vote(req).await)
    }

    async fn snapshot(
        &self,
        _from: NodeId,
        to: NodeId,
        vote: Vote,
        meta: SnapshotMeta,
        data: SM::SnapshotData,
    ) -> Result<Vote, Unreachable> {
        delivered(self.node(to)?.handle_snapshot(vote, meta, data).await)
    }

    async fn transfer_leader(
        &self,
        _from: NodeId,
        to: NodeId,
        req: TransferLeader,
    ) -> Result<TransferReply, Unreachable> {
        delivered(self.node(to)?.handle_transfer_leader(req).await)
    }
}
