//! The network adapter: openraft's network traits over [`Transport`].
//!
//! openraft asks a factory for one client per peer and sends requests in
//! its own types. The adapter converts each request to the transport's
//! messages, bounds it by openraft's deadline, and converts the reply back.

use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use openraft::AsyncRuntime;
use openraft::RaftNetworkFactory;
use openraft::error::NetworkError;
use openraft::error::RPCError;
use openraft::error::ReplicationClosed;
use openraft::error::StreamingError;
use openraft::error::Unreachable as OrUnreachable;
use openraft::network::RPCOption;
use openraft::network::v2::RaftNetworkV2;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::AppendResult;
use openraft::raft::SnapshotResponse;
use openraft::raft::TransferLeaderRequest;
use openraft::raft::TransferLeaderResponse;
use openraft::raft::VoteRequest as OrVoteRequest;
use openraft::raft::VoteResponse as OrVoteResponse;
use openraft::storage::Snapshot;

use crate::transport::AppendEntries;
use crate::transport::AppendReply;
use crate::transport::SnapshotMeta;
use crate::transport::TransferLeader;
use crate::transport::TransferReply;
use crate::transport::Transport;
use crate::transport::Unreachable;
use crate::transport::VoteReply;
use crate::transport::VoteRequest;
use crate::types::CommittedLeaderId;
use crate::types::Membership;
use crate::types::NodeId;
use crate::types::OrVote;
use crate::types::PeerInfo;
use crate::types::Types;

/// Creates one [`Peer`] client per target node.
pub(crate) struct Factory<T> {
    pub(crate) id: NodeId,
    pub(crate) transport: Arc<T>,
}

/// openraft's client for one peer.
pub(crate) struct Peer<T, RT> {
    from: NodeId,
    to: NodeId,
    transport: Arc<T>,
    _rt: PhantomData<fn() -> RT>,
}

impl<RT: AsyncRuntime, T: Transport> RaftNetworkFactory<Types<RT>> for Factory<T> {
    type Network = Peer<T, RT>;

    async fn new_client(&mut self, target: NodeId, _node: &PeerInfo) -> Self::Network {
        Peer {
            from: self.id,
            to: target,
            transport: Arc::clone(&self.transport),
            _rt: PhantomData,
        }
    }
}

fn unreachable<RT: AsyncRuntime>(error: &Unreachable) -> OrUnreachable<Types<RT>> {
    OrUnreachable::new(error)
}

/// Run `call` within openraft's deadline. A transport that loses a message
/// may never answer; the deadline turns that into `Unreachable`.
async fn bounded<RT: AsyncRuntime, R>(
    option: &RPCOption,
    call: impl Future<Output = Result<R, Unreachable>> + Send,
) -> Result<R, Unreachable> {
    RT::timeout(option.hard_ttl(), call)
        .await
        .unwrap_or_else(|_| Err(Unreachable("no reply before the deadline".into())))
}

impl<RT: AsyncRuntime, T: Transport> RaftNetworkV2<Types<RT>> for Peer<T, RT> {
    type SnapshotData = T::SnapshotData;

    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<Types<RT>>,
        option: RPCOption,
    ) -> Result<AppendResult<Types<RT>>, RPCError<Types<RT>>> {
        let req = AppendEntries::try_from(rpc)
            .map_err(|error| RPCError::Network(NetworkError::new(&error)))?;
        let reply: AppendReply = bounded::<RT, _>(
            &option,
            self.transport.append_entries(self.from, self.to, req),
        )
        .await
        .map_err(|error| RPCError::Unreachable(unreachable::<RT>(&error)))?;
        Ok(reply.into())
    }

    async fn vote(
        &mut self,
        rpc: OrVoteRequest<Types<RT>>,
        option: RPCOption,
    ) -> Result<OrVoteResponse<Types<RT>>, RPCError<Types<RT>>> {
        let req = VoteRequest::from(rpc);
        let reply: VoteReply =
            bounded::<RT, _>(&option, self.transport.vote(self.from, self.to, req))
                .await
                .map_err(|error| RPCError::Unreachable(unreachable::<RT>(&error)))?;
        Ok(reply.into())
    }

    async fn pre_vote(
        &mut self,
        rpc: OrVoteRequest<Types<RT>>,
        option: RPCOption,
    ) -> Result<OrVoteResponse<Types<RT>>, RPCError<Types<RT>>> {
        let req = VoteRequest::from(rpc);
        let reply: VoteReply =
            bounded::<RT, _>(&option, self.transport.pre_vote(self.from, self.to, req))
                .await
                .map_err(|error| RPCError::Unreachable(unreachable::<RT>(&error)))?;
        Ok(reply.into())
    }

    async fn full_snapshot(
        &mut self,
        vote: OrVote,
        snapshot: Snapshot<CommittedLeaderId, Membership, T::SnapshotData>,
        _cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<Types<RT>>, StreamingError<Types<RT>>> {
        let meta = SnapshotMeta::from(&snapshot.meta);
        let call =
            self.transport
                .snapshot(self.from, self.to, vote.into(), meta, snapshot.snapshot);
        let vote = bounded::<RT, _>(&option, call)
            .await
            .map_err(|error| StreamingError::Unreachable(unreachable::<RT>(&error)))?;
        Ok(SnapshotResponse::new(vote.into()))
    }

    async fn transfer_leader(
        &mut self,
        req: TransferLeaderRequest<Types<RT>>,
        option: RPCOption,
    ) -> Result<TransferLeaderResponse<Types<RT>>, RPCError<Types<RT>>> {
        let req = TransferLeader::from(&req);
        let reply: TransferReply = bounded::<RT, _>(
            &option,
            self.transport.transfer_leader(self.from, self.to, req),
        )
        .await
        .map_err(|error| RPCError::Unreachable(unreachable::<RT>(&error)))?;
        Ok(reply.into())
    }
}
