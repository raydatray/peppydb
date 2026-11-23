//! Peer-to-peer Raft gRPC transport (`peppydb.raft.v1`, plan 0032).
//!
//! Implements [`server_raft::Transport`] over tonic (`GrpcTransport`) and
//! serves incoming peer RPCs (`RaftRpcServer`) by forwarding them to
//! [`server_raft::RaftNode`]. `server-raft` never depends on tonic or
//! `raft-proto`.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use futures_util::StreamExt;
use raft_proto::raft::v1::raft_service_client::RaftServiceClient;
use raft_proto::raft::v1::raft_service_server::RaftService;
use raft_proto::raft::v1::{
    self as pb, AcceptedReply, AppendEntriesRequest, AppendEntriesResponse, BlankPayload,
    ConflictReply, InstallSnapshotHeader, InstallSnapshotRequest, InstallSnapshotResponse,
    LogNotFlushed, PreVoteRequest, PreVoteResponse, TransferAccepted, TransferLeaderRequest,
    TransferLeaderResponse, VoteChanged, VoteRequest as PbVoteRequest, VoteResponse, VoterSet,
    append_entries_response, entry, install_snapshot_request, transfer_leader_response,
};
use server_raft::transport::{
    AppendEntries, AppendReply, Entry, EntryPayload, LogId, MembershipConfig, SnapshotMeta,
    TransferLeader, TransferReply, Transport, Unreachable, Vote, VoteReply, VoteRequest,
};
use server_raft::{DbStateMachine, HandleError, NodeId, PeerInfo, RaftNode};
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Response, Status, Streaming};

/// Default chunk size for client-streamed `InstallSnapshot` requests.
pub(crate) const SNAPSHOT_CHUNK_BYTES: usize = 256 << 10;

/// Tonic-backed implementation of [`server_raft::Transport`].
pub(crate) struct GrpcTransport {
    peers: RwLock<BTreeMap<NodeId, (String, Channel)>>,
    chunk_bytes: usize,
}

impl GrpcTransport {
    pub(crate) fn new(peers: &BTreeMap<NodeId, PeerInfo>) -> Result<Self, String> {
        let mut map = BTreeMap::new();
        for (&id, info) in peers {
            let channel = lazy_channel(&info.addr)?;
            map.insert(id, (info.addr.clone(), channel));
        }
        Ok(Self {
            peers: RwLock::new(map),
            chunk_bytes: SNAPSHOT_CHUNK_BYTES,
        })
    }

    fn channel(&self, to: NodeId) -> Result<Channel, Unreachable> {
        let guard = self.peers.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(&to)
            .map(|(_, ch)| ch.clone())
            .ok_or_else(|| Unreachable(format!("unknown peer {to}")))
    }

    #[cfg(test)]
    pub(crate) fn with_chunk_bytes(mut self, chunk_bytes: usize) -> Self {
        self.chunk_bytes = chunk_bytes;
        self
    }
}

fn lazy_channel(addr: &str) -> Result<Channel, String> {
    let uri = if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_owned()
    } else {
        format!("http://{addr}")
    };
    let endpoint =
        Endpoint::from_shared(uri).map_err(|e| format!("invalid peer address {addr:?}: {e}"))?;
    Ok(endpoint.connect_lazy())
}

impl Transport for GrpcTransport {
    type SnapshotData = Vec<u8>;

    async fn append_entries(
        &self,
        from: NodeId,
        to: NodeId,
        req: AppendEntries,
    ) -> Result<AppendReply, Unreachable> {
        let ch = self.channel(to)?;
        let mut client = RaftServiceClient::new(ch);
        let pb_req = AppendEntriesRequest {
            from,
            vote: Some(pb_vote(req.vote)),
            prev: req.prev.map(pb_log_id),
            entries: req.entries.into_iter().map(pb_entry).collect(),
            commit: req.commit.map(pb_log_id),
        };
        let resp = client
            .append_entries(pb_req)
            .await
            .map_err(|s| Unreachable(s.to_string()))?
            .into_inner();
        from_pb_append_reply(resp).map_err(Unreachable)
    }

    async fn vote(
        &self,
        from: NodeId,
        to: NodeId,
        req: VoteRequest,
    ) -> Result<VoteReply, Unreachable> {
        let ch = self.channel(to)?;
        let mut client = RaftServiceClient::new(ch);
        let pb_req = PbVoteRequest {
            from,
            vote: Some(pb_vote(req.vote)),
            last: req.last.map(pb_log_id),
            transfer_from: req.transfer_from.map(pb_vote),
        };
        let resp = client
            .vote(pb_req)
            .await
            .map_err(|s| Unreachable(s.to_string()))?
            .into_inner();
        from_pb_vote_reply(resp.vote, resp.granted, resp.last).map_err(Unreachable)
    }

    async fn pre_vote(
        &self,
        from: NodeId,
        to: NodeId,
        req: VoteRequest,
    ) -> Result<VoteReply, Unreachable> {
        let ch = self.channel(to)?;
        let mut client = RaftServiceClient::new(ch);
        let pb_req = PreVoteRequest {
            from,
            vote: Some(pb_vote(req.vote)),
            last: req.last.map(pb_log_id),
            transfer_from: req.transfer_from.map(pb_vote),
        };
        let resp = client
            .pre_vote(pb_req)
            .await
            .map_err(|s| Unreachable(s.to_string()))?
            .into_inner();
        from_pb_vote_reply(resp.vote, resp.granted, resp.last).map_err(Unreachable)
    }

    async fn snapshot(
        &self,
        from: NodeId,
        to: NodeId,
        vote: Vote,
        meta: SnapshotMeta,
        data: Self::SnapshotData,
    ) -> Result<Vote, Unreachable> {
        let ch = self.channel(to)?;
        let mut client = RaftServiceClient::new(ch);
        let header = InstallSnapshotRequest {
            payload: Some(install_snapshot_request::Payload::Header(
                InstallSnapshotHeader {
                    from,
                    vote: Some(pb_vote(vote)),
                    meta: Some(pb_snapshot_meta(meta)),
                },
            )),
        };
        let chunk_size = self.chunk_bytes.max(1);
        let mut messages = Vec::with_capacity(1 + data.len().div_ceil(chunk_size));
        messages.push(header);
        for chunk in data.chunks(chunk_size) {
            messages.push(InstallSnapshotRequest {
                payload: Some(install_snapshot_request::Payload::Chunk(chunk.to_vec())),
            });
        }
        let resp = client
            .install_snapshot(futures_util::stream::iter(messages))
            .await
            .map_err(|s| Unreachable(s.to_string()))?
            .into_inner();
        resp.vote
            .map(from_pb_vote)
            .ok_or_else(|| Unreachable("missing vote in InstallSnapshotResponse".to_owned()))
    }

    async fn transfer_leader(
        &self,
        from: NodeId,
        to: NodeId,
        req: TransferLeader,
    ) -> Result<TransferReply, Unreachable> {
        let ch = self.channel(to)?;
        let mut client = RaftServiceClient::new(ch);
        let pb_req = TransferLeaderRequest {
            from_node: from,
            from_vote: Some(pb_vote(req.from)),
            to: req.to,
            last: req.last.map(pb_log_id),
        };
        let resp = client
            .transfer_leader(pb_req)
            .await
            .map_err(|s| Unreachable(s.to_string()))?
            .into_inner();
        from_pb_transfer_reply(resp).map_err(Unreachable)
    }
}

/// Incoming `peppydb.raft.v1.RaftService` handler delegating to [`RaftNode`].
pub(crate) struct RaftRpcServer {
    pub(crate) node: Arc<RaftNode<DbStateMachine>>,
}

#[tonic::async_trait]
impl RaftService for RaftRpcServer {
    async fn append_entries(
        &self,
        request: Request<AppendEntriesRequest>,
    ) -> Result<Response<AppendEntriesResponse>, Status> {
        let req = from_pb_append_entries(request.into_inner()).map_err(Status::invalid_argument)?;
        let reply = self
            .node
            .handle_append_entries(req)
            .await
            .map_err(handle_status)?;
        Ok(Response::new(pb_append_reply(reply)))
    }

    async fn vote(
        &self,
        request: Request<PbVoteRequest>,
    ) -> Result<Response<VoteResponse>, Status> {
        let pb = request.into_inner();
        let req = from_pb_vote_request(pb.vote, pb.last, pb.transfer_from)
            .map_err(Status::invalid_argument)?;
        let reply = self.node.handle_vote(req).await.map_err(handle_status)?;
        Ok(Response::new(VoteResponse {
            vote: Some(pb_vote(reply.vote)),
            granted: reply.granted,
            last: reply.last.map(pb_log_id),
        }))
    }

    async fn pre_vote(
        &self,
        request: Request<PreVoteRequest>,
    ) -> Result<Response<PreVoteResponse>, Status> {
        let pb = request.into_inner();
        let req = from_pb_vote_request(pb.vote, pb.last, pb.transfer_from)
            .map_err(Status::invalid_argument)?;
        let reply = self
            .node
            .handle_pre_vote(req)
            .await
            .map_err(handle_status)?;
        Ok(Response::new(PreVoteResponse {
            vote: Some(pb_vote(reply.vote)),
            granted: reply.granted,
            last: reply.last.map(pb_log_id),
        }))
    }

    async fn install_snapshot(
        &self,
        request: Request<Streaming<InstallSnapshotRequest>>,
    ) -> Result<Response<InstallSnapshotResponse>, Status> {
        let mut stream = request.into_inner();
        let first = stream
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("empty InstallSnapshot stream"))??;
        let header = match first.payload {
            Some(install_snapshot_request::Payload::Header(h)) => h,
            _ => {
                return Err(Status::invalid_argument(
                    "first InstallSnapshot message must be a header",
                ));
            }
        };
        let vote = header
            .vote
            .map(from_pb_vote)
            .ok_or_else(|| Status::invalid_argument("missing vote in InstallSnapshotHeader"))?;
        let meta = header
            .meta
            .ok_or_else(|| Status::invalid_argument("missing meta in InstallSnapshotHeader"))?;
        let meta = from_pb_snapshot_meta(meta).map_err(Status::invalid_argument)?;

        let mut data = Vec::new();
        while let Some(msg) = stream.next().await {
            let msg = msg?;
            match msg.payload {
                Some(install_snapshot_request::Payload::Chunk(chunk)) => {
                    data.extend_from_slice(&chunk);
                }
                _ => {
                    return Err(Status::invalid_argument(
                        "unexpected non-chunk message in InstallSnapshot stream",
                    ));
                }
            }
        }
        let reply_vote = self
            .node
            .handle_snapshot(vote, meta, data)
            .await
            .map_err(handle_status)?;
        Ok(Response::new(InstallSnapshotResponse {
            vote: Some(pb_vote(reply_vote)),
        }))
    }

    async fn transfer_leader(
        &self,
        request: Request<TransferLeaderRequest>,
    ) -> Result<Response<TransferLeaderResponse>, Status> {
        let pb = request.into_inner();
        let req = TransferLeader {
            from: pb
                .from_vote
                .map(from_pb_vote)
                .ok_or_else(|| Status::invalid_argument("missing from_vote"))?,
            to: pb.to,
            last: pb.last.map(from_pb_log_id),
        };
        let reply = self
            .node
            .handle_transfer_leader(req)
            .await
            .map_err(handle_status)?;
        Ok(Response::new(pb_transfer_reply(reply)))
    }
}

fn handle_status(error: HandleError) -> Status {
    match error {
        HandleError::Stopped(msg) => Status::unavailable(msg),
        HandleError::BadMessage(err) => Status::invalid_argument(err.to_string()),
    }
}

// --- Proto conversions.

fn pb_log_id(id: LogId) -> pb::LogId {
    pb::LogId {
        term: id.term,
        index: id.index,
    }
}

fn from_pb_log_id(id: pb::LogId) -> LogId {
    LogId {
        term: id.term,
        index: id.index,
    }
}

fn pb_vote(v: Vote) -> pb::Vote {
    pb::Vote {
        term: v.term,
        node: v.node,
        committed: v.committed,
    }
}

fn from_pb_vote(v: pb::Vote) -> Vote {
    Vote {
        term: v.term,
        node: v.node,
        committed: v.committed,
    }
}

fn pb_membership(m: MembershipConfig) -> pb::MembershipConfig {
    pb::MembershipConfig {
        configs: m
            .configs
            .into_iter()
            .map(|set| VoterSet {
                nodes: set.into_iter().collect(),
            })
            .collect(),
        nodes: m
            .nodes
            .into_iter()
            .map(|(id, info)| pb::PeerNode {
                id,
                addr: info.addr,
            })
            .collect(),
    }
}

fn from_pb_membership(m: pb::MembershipConfig) -> MembershipConfig {
    MembershipConfig {
        configs: m
            .configs
            .into_iter()
            .map(|vs| vs.nodes.into_iter().collect::<BTreeSet<_>>())
            .collect(),
        nodes: m
            .nodes
            .into_iter()
            .map(|n| (n.id, PeerInfo { addr: n.addr }))
            .collect(),
    }
}

fn pb_entry(e: Entry) -> pb::Entry {
    let payload = match e.payload {
        EntryPayload::Blank => entry::Payload::Blank(BlankPayload {}),
        EntryPayload::Command(bytes) => entry::Payload::Command(bytes),
        EntryPayload::Membership(m) => entry::Payload::Membership(pb_membership(m)),
    };
    pb::Entry {
        log_id: Some(pb_log_id(e.log_id)),
        payload: Some(payload),
    }
}

fn from_pb_entry(e: pb::Entry) -> Result<Entry, String> {
    let log_id = e
        .log_id
        .map(from_pb_log_id)
        .ok_or_else(|| "missing log_id in Entry".to_owned())?;
    let payload = match e.payload {
        Some(entry::Payload::Blank(_)) => EntryPayload::Blank,
        Some(entry::Payload::Command(bytes)) => EntryPayload::Command(bytes),
        Some(entry::Payload::Membership(m)) => EntryPayload::Membership(from_pb_membership(m)),
        None => return Err("missing payload in Entry".to_owned()),
    };
    Ok(Entry { log_id, payload })
}

fn from_pb_append_entries(req: AppendEntriesRequest) -> Result<AppendEntries, String> {
    let vote = req
        .vote
        .map(from_pb_vote)
        .ok_or_else(|| "missing vote in AppendEntriesRequest".to_owned())?;
    let entries: Result<Vec<_>, _> = req.entries.into_iter().map(from_pb_entry).collect();
    Ok(AppendEntries {
        vote,
        prev: req.prev.map(from_pb_log_id),
        entries: entries?,
        commit: req.commit.map(from_pb_log_id),
    })
}

fn pb_append_reply(reply: AppendReply) -> AppendEntriesResponse {
    let r = match reply {
        AppendReply::Accepted(last) => append_entries_response::Reply::Accepted(AcceptedReply {
            last: last.map(pb_log_id),
        }),
        AppendReply::Conflict { log_id, hint } => {
            let (has_hint, hint_vec) = match hint {
                Some(v) => (true, v.into_iter().map(pb_log_id).collect()),
                None => (false, Vec::new()),
            };
            append_entries_response::Reply::Conflict(ConflictReply {
                log_id: Some(pb_log_id(log_id)),
                has_hint,
                hint: hint_vec,
            })
        }
        AppendReply::HigherVote(v) => append_entries_response::Reply::HigherVote(pb_vote(v)),
    };
    AppendEntriesResponse { reply: Some(r) }
}

fn from_pb_append_reply(resp: AppendEntriesResponse) -> Result<AppendReply, String> {
    match resp.reply {
        Some(append_entries_response::Reply::Accepted(a)) => {
            Ok(AppendReply::Accepted(a.last.map(from_pb_log_id)))
        }
        Some(append_entries_response::Reply::Conflict(c)) => {
            let log_id = c
                .log_id
                .map(from_pb_log_id)
                .ok_or_else(|| "missing log_id in ConflictReply".to_owned())?;
            let hint = if c.has_hint {
                Some(c.hint.into_iter().map(from_pb_log_id).collect())
            } else {
                None
            };
            Ok(AppendReply::Conflict { log_id, hint })
        }
        Some(append_entries_response::Reply::HigherVote(v)) => {
            Ok(AppendReply::HigherVote(from_pb_vote(v)))
        }
        None => Err("missing reply in AppendEntriesResponse".to_owned()),
    }
}

fn from_pb_vote_request(
    vote: Option<pb::Vote>,
    last: Option<pb::LogId>,
    transfer_from: Option<pb::Vote>,
) -> Result<VoteRequest, String> {
    Ok(VoteRequest {
        vote: vote
            .map(from_pb_vote)
            .ok_or_else(|| "missing vote in VoteRequest".to_owned())?,
        last: last.map(from_pb_log_id),
        transfer_from: transfer_from.map(from_pb_vote),
    })
}

fn from_pb_vote_reply(
    vote: Option<pb::Vote>,
    granted: bool,
    last: Option<pb::LogId>,
) -> Result<VoteReply, String> {
    Ok(VoteReply {
        vote: vote
            .map(from_pb_vote)
            .ok_or_else(|| "missing vote in VoteResponse".to_owned())?,
        granted,
        last: last.map(from_pb_log_id),
    })
}

fn pb_snapshot_meta(meta: SnapshotMeta) -> pb::SnapshotMeta {
    pb::SnapshotMeta {
        last: meta.last.map(pb_log_id),
        membership_log_id: meta.membership_log_id.map(pb_log_id),
        membership: Some(pb_membership(meta.membership)),
    }
}

fn from_pb_snapshot_meta(meta: pb::SnapshotMeta) -> Result<SnapshotMeta, String> {
    let membership = meta
        .membership
        .map(from_pb_membership)
        .ok_or_else(|| "missing membership in SnapshotMeta".to_owned())?;
    Ok(SnapshotMeta {
        last: meta.last.map(from_pb_log_id),
        membership_log_id: meta.membership_log_id.map(from_pb_log_id),
        membership,
    })
}

fn pb_transfer_reply(reply: TransferReply) -> TransferLeaderResponse {
    let r = match reply {
        TransferReply::Accepted => transfer_leader_response::Reply::Accepted(TransferAccepted {}),
        TransferReply::VoteChanged { expected, actual } => {
            transfer_leader_response::Reply::VoteChanged(VoteChanged {
                expected: Some(pb_vote(expected)),
                actual: Some(pb_vote(actual)),
            })
        }
        TransferReply::LogNotFlushed { expected, actual } => {
            transfer_leader_response::Reply::LogNotFlushed(LogNotFlushed {
                expected: expected.map(pb_log_id),
                actual: actual.map(pb_log_id),
            })
        }
    };
    TransferLeaderResponse { reply: Some(r) }
}

fn from_pb_transfer_reply(resp: TransferLeaderResponse) -> Result<TransferReply, String> {
    match resp.reply {
        Some(transfer_leader_response::Reply::Accepted(_)) => Ok(TransferReply::Accepted),
        Some(transfer_leader_response::Reply::VoteChanged(v)) => Ok(TransferReply::VoteChanged {
            expected: v
                .expected
                .map(from_pb_vote)
                .ok_or_else(|| "missing expected vote".to_owned())?,
            actual: v
                .actual
                .map(from_pb_vote)
                .ok_or_else(|| "missing actual vote".to_owned())?,
        }),
        Some(transfer_leader_response::Reply::LogNotFlushed(l)) => {
            Ok(TransferReply::LogNotFlushed {
                expected: l.expected.map(from_pb_log_id),
                actual: l.actual.map(from_pb_log_id),
            })
        }
        None => Err("missing reply in TransferLeaderResponse".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use raft_proto::raft::v1::raft_service_server::RaftServiceServer;
    use server_raft::{Command, DiskLog, LogOptions, Op, RaftConfig, Role};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tonic::transport::server::TcpIncoming;

    use super::*;

    #[test]
    fn proto_conversions_round_trip() {
        let log_id = LogId { term: 3, index: 12 };
        let vote = Vote {
            term: 4,
            node: 2,
            committed: true,
        };
        let membership = MembershipConfig {
            configs: vec![BTreeSet::from([1, 2, 3])],
            nodes: BTreeMap::from([
                (
                    1,
                    PeerInfo {
                        addr: "127.0.0.1:7001".to_owned(),
                    },
                ),
                (
                    2,
                    PeerInfo {
                        addr: "127.0.0.1:7002".to_owned(),
                    },
                ),
            ]),
        };
        let append = AppendEntries {
            vote,
            prev: Some(log_id),
            entries: vec![
                Entry {
                    log_id: LogId { term: 4, index: 13 },
                    payload: EntryPayload::Blank,
                },
                Entry {
                    log_id: LogId { term: 4, index: 14 },
                    payload: EntryPayload::Command(vec![1, 2, 3, 4]),
                },
                Entry {
                    log_id: LogId { term: 4, index: 15 },
                    payload: EntryPayload::Membership(membership.clone()),
                },
            ],
            commit: Some(log_id),
        };
        let pb_append = AppendEntriesRequest {
            from: 1,
            vote: Some(pb_vote(append.vote)),
            prev: append.prev.map(pb_log_id),
            entries: append.entries.clone().into_iter().map(pb_entry).collect(),
            commit: append.commit.map(pb_log_id),
        };
        assert_eq!(from_pb_append_entries(pb_append).unwrap(), append);

        for reply in [
            AppendReply::Accepted(None),
            AppendReply::Accepted(Some(log_id)),
            AppendReply::Conflict { log_id, hint: None },
            AppendReply::Conflict {
                log_id,
                hint: Some(vec![log_id]),
            },
            AppendReply::HigherVote(vote),
        ] {
            assert_eq!(
                from_pb_append_reply(pb_append_reply(reply.clone())).unwrap(),
                reply
            );
        }

        let meta = SnapshotMeta {
            last: Some(log_id),
            membership_log_id: Some(LogId { term: 1, index: 0 }),
            membership,
        };
        assert_eq!(
            from_pb_snapshot_meta(pb_snapshot_meta(meta.clone())).unwrap(),
            meta
        );

        for reply in [
            TransferReply::Accepted,
            TransferReply::VoteChanged {
                expected: vote,
                actual: Vote { term: 5, ..vote },
            },
            TransferReply::LogNotFlushed {
                expected: Some(log_id),
                actual: None,
            },
        ] {
            assert_eq!(
                from_pb_transfer_reply(pb_transfer_reply(reply.clone())).unwrap(),
                reply
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn three_nodes_elect_replicate_and_transfer_leader_over_grpc() {
        let mut listeners = Vec::new();
        let mut peers = BTreeMap::new();
        for id in 1..=3u64 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            peers.insert(id, PeerInfo { addr });
            listeners.push((id, listener));
        }

        let mut dirs = Vec::new();
        let mut nodes = BTreeMap::new();
        let mut stops = Vec::new();
        let mut tasks = Vec::new();
        let mut sms = BTreeMap::new();

        for (id, listener) in listeners {
            let dir = tempfile::tempdir().unwrap();
            let log = DiskLog::open_real(&dir.path().join("log"), LogOptions::default()).unwrap();
            let sync = log.sync_hook();
            let sm = DbStateMachine::open_dir(&dir.path().join("db"), false, sync).unwrap();
            let db_handle = sm.clone();
            let transport = Arc::new(GrpcTransport::new(&peers).unwrap().with_chunk_bytes(4096));
            let cfg = RaftConfig {
                snapshot_after: Some(20),
                keep_below_snapshot: 5,
                ..RaftConfig::default()
            };
            let node = Arc::new(RaftNode::start(id, &cfg, log, sm, transport).await.unwrap());
            let (stop_tx, stop_rx) = oneshot::channel::<()>();
            let rpc = RaftRpcServer {
                node: Arc::clone(&node),
            };
            let task = tokio::spawn(async move {
                let _ = tonic::transport::Server::builder()
                    .add_service(RaftServiceServer::new(rpc))
                    .serve_with_incoming_shutdown(TcpIncoming::from(listener), async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
            dirs.push(dir);
            nodes.insert(id, node);
            sms.insert(id, db_handle);
            stops.push(stop_tx);
            tasks.push(task);
        }

        nodes[&1].initialize(peers.clone()).await.unwrap();

        // Wait for a leader to be elected and confirm leadership.
        let leader_id = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                for (&id, node) in &nodes {
                    if node.status().role == Role::Leader && node.read_barrier().await.is_ok() {
                        return id;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("leader elected");

        // Propose enough writes to trigger a snapshot and replicate across gRPC.
        for i in 0..30u32 {
            let key = format!("\x01k{i:03}").into_bytes();
            let val = format!("v{i:03}").into_bytes();
            let cmd = Command {
                ops: vec![Op::Put { key, value: val }],
            };
            nodes[&leader_id].propose(cmd).await.unwrap();
        }
        nodes[&leader_id].read_barrier().await.unwrap();

        // Transfer leadership to another node over gRPC.
        let target = if leader_id == 1 { 2 } else { 1 };
        nodes[&leader_id].transfer_leader(target).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if nodes[&target].status().role == Role::Leader {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("leadership transferred");

        // Verify the new leader has the replicated key in its engine Db.
        let got = sms[&target].db().unwrap().get(b"\x01k029").unwrap();
        assert_eq!(got.as_deref(), Some(&b"v029"[..]));

        for stop in stops {
            let _ = stop.send(());
        }
        for (_, node) in nodes {
            node.shutdown().await;
        }
        for task in tasks {
            let _ = task.await;
        }
    }
}
