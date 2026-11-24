//! The `AdminService` service: status, checkpoints, and snapshots.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use futures_util::Stream;
use kv_proto::kv::v1::admin_service_server::AdminService;
use kv_proto::kv::v1::snapshot_response::Payload;
use kv_proto::kv::v1::{
    AddLearnerRequest, AddLearnerResponse, ChangeMembershipRequest, ChangeMembershipResponse,
    CheckpointRequest, CheckpointResponse, MemberInfo, SnapshotRequest, SnapshotResponse,
    SnapshotTrailer, StatusRequest, StatusResponse, TransferLeaderRequest, TransferLeaderResponse,
};
use tokio::sync::mpsc;
use tonic::{Code, Request, Response, Status};

use crate::VERSION;
use crate::bridge::{Inner, role_str};
use crate::metrics::{Method, result_code};

/// Bounded chunks in flight between the blocking `write_snapshot` call and the
/// gRPC stream. Provides backpressure against slow clients.
const SNAPSHOT_CHANNEL_CAPACITY: usize = 4;

pub(crate) struct Admin {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct SnapshotStream {
    rx: mpsc::Receiver<Result<SnapshotResponse, Status>>,
}

impl Stream for SnapshotStream {
    type Item = Result<SnapshotResponse, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// Bridges `Db::write_snapshot`'s synchronous `io::Write` to the bounded gRPC
/// response channel, tracking total bytes and a running Castagnoli CRC32C.
struct ChannelWriter {
    tx: mpsc::Sender<Result<SnapshotResponse, Status>>,
    inner: Arc<Inner>,
    chunk_size: usize,
    buf: Vec<u8>,
    crc: u32,
    total_bytes: u64,
    cancelled: bool,
}

impl ChannelWriter {
    fn new(
        tx: mpsc::Sender<Result<SnapshotResponse, Status>>,
        inner: Arc<Inner>,
        chunk_size: usize,
    ) -> Self {
        Self {
            tx,
            inner,
            chunk_size,
            buf: Vec::with_capacity(chunk_size),
            crc: 0,
            total_bytes: 0,
            cancelled: false,
        }
    }

    fn send_chunk(&mut self, chunk: Vec<u8>) -> io::Result<()> {
        let len = chunk.len() as u64;
        self.crc = crc32c::crc32c_append(self.crc, &chunk);
        self.total_bytes = self.total_bytes.saturating_add(len);
        self.tx
            .blocking_send(Ok(SnapshotResponse {
                payload: Some(Payload::Chunk(chunk)),
            }))
            .map_err(|_| {
                self.cancelled = true;
                io::Error::new(io::ErrorKind::BrokenPipe, "snapshot client disconnected")
            })?;
        self.inner.metrics.add_snapshot_bytes(len);
        Ok(())
    }

    fn finish(mut self, checkpoint: engine::Checkpoint) -> bool {
        if !self.buf.is_empty() {
            let chunk = std::mem::take(&mut self.buf);
            if self.send_chunk(chunk).is_err() {
                return false;
            }
        }
        let trailer = SnapshotTrailer {
            applied_seq: checkpoint.applied_seq,
            checkpoint_id: checkpoint.checkpoint_id,
            total_bytes: self.total_bytes,
            crc32c: self.crc,
        };
        self.tx
            .blocking_send(Ok(SnapshotResponse {
                payload: Some(Payload::Trailer(trailer)),
            }))
            .is_ok()
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, mut data: &[u8]) -> io::Result<usize> {
        let len = data.len();
        while !data.is_empty() {
            let space = self.chunk_size - self.buf.len();
            let take = space.min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == self.chunk_size {
                let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(self.chunk_size));
                self.send_chunk(chunk)?;
            }
        }
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tonic::async_trait]
impl AdminService for Admin {
    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let started = Instant::now();
        let result = async {
            let applied_seq = self.inner.blocking(|db| Ok(db.applied_seq())).await?;
            let raft = self.inner.raft_status();
            let leader_addr = raft.leader.and_then(|id| self.inner.leader_addr(id));
            let voter_ids: BTreeSet<u64> = raft
                .membership
                .configs
                .iter()
                .flat_map(|cfg| cfg.iter().copied())
                .collect();
            let mut voters = Vec::new();
            let mut learners = Vec::new();
            for (node_id, peer) in &raft.membership.nodes {
                let match_index = raft
                    .replication
                    .get(node_id)
                    .and_then(|opt| opt.map(|id| id.index));
                let info = MemberInfo {
                    node_id: *node_id,
                    addr: peer.addr.clone(),
                    match_index,
                };
                if voter_ids.contains(node_id) {
                    voters.push(info);
                } else {
                    learners.push(info);
                }
            }
            Ok(Response::new(StatusResponse {
                applied_seq,
                version: VERSION.to_owned(),
                node_id: raft.id,
                role: role_str(raft.role).to_owned(),
                term: raft.term,
                leader_id: raft.leader,
                leader_addr,
                voters,
                learners,
            }))
        }
        .await;
        self.inner
            .metrics
            .record_rpc(Method::Status, result_code(&result), started.elapsed());
        result
    }

    async fn checkpoint(
        &self,
        _request: Request<CheckpointRequest>,
    ) -> Result<Response<CheckpointResponse>, Status> {
        let started = Instant::now();
        let result = async {
            let checkpoint = self.inner.checkpoint().await?;
            Ok(Response::new(CheckpointResponse {
                applied_seq: checkpoint.applied_seq,
                checkpoint_id: checkpoint.checkpoint_id,
            }))
        }
        .await;
        self.inner
            .metrics
            .record_rpc(Method::Checkpoint, result_code(&result), started.elapsed());
        result
    }

    type SnapshotStream = SnapshotStream;

    async fn snapshot(
        &self,
        request: Request<SnapshotRequest>,
    ) -> Result<Response<Self::SnapshotStream>, Status> {
        let started = Instant::now();
        let checkpoint_first = request.into_inner().checkpoint;
        let chunk_size = self.inner.config.snapshot_chunk_bytes;
        let (tx, rx) = mpsc::channel(SNAPSHOT_CHANNEL_CAPACITY);
        let inner = Arc::clone(&self.inner);
        if let Err(status) = self
            .inner
            .spawn_snapshot(move |sm| {
                if checkpoint_first && let Err(error) = sm.checkpoint_now() {
                    let status = inner.engine_status(error);
                    inner
                        .metrics
                        .record_rpc(Method::Snapshot, status.code(), started.elapsed());
                    let _ = tx.blocking_send(Err(status));
                    return;
                }
                let db = match sm.db() {
                    Ok(db) => db,
                    Err(error) => {
                        let status = Status::internal(error.to_string());
                        inner.metrics.record_rpc(
                            Method::Snapshot,
                            status.code(),
                            started.elapsed(),
                        );
                        let _ = tx.blocking_send(Err(status));
                        return;
                    }
                };
                let mut writer = ChannelWriter::new(tx.clone(), Arc::clone(&inner), chunk_size);
                let code = match db.write_snapshot(&mut writer) {
                    Ok(checkpoint) => {
                        if writer.finish(checkpoint) {
                            Code::Ok
                        } else {
                            Code::Cancelled
                        }
                    }
                    Err(_) if writer.cancelled => Code::Cancelled,
                    Err(error) => {
                        let status = inner.engine_status(error);
                        let code = status.code();
                        let _ = tx.blocking_send(Err(status));
                        code
                    }
                };
                inner
                    .metrics
                    .record_rpc(Method::Snapshot, code, started.elapsed());
            })
            .await
        {
            self.inner
                .metrics
                .record_rpc(Method::Snapshot, status.code(), started.elapsed());
            return Err(status);
        }
        Ok(Response::new(SnapshotStream { rx }))
    }

    async fn add_learner(
        &self,
        request: Request<AddLearnerRequest>,
    ) -> Result<Response<AddLearnerResponse>, Status> {
        let started = Instant::now();
        let req = request.into_inner();
        let result = async {
            if req.node_id == 0 {
                return Err(Status::invalid_argument("node_id must be non-zero"));
            }
            let addr = req.addr.trim().to_owned();
            if addr.is_empty() {
                return Err(Status::invalid_argument("addr must not be empty"));
            }
            self.inner.add_learner(req.node_id, addr).await?;
            Ok(Response::new(AddLearnerResponse {}))
        }
        .await;
        self.inner
            .metrics
            .record_rpc(Method::AddLearner, result_code(&result), started.elapsed());
        result
    }

    async fn change_membership(
        &self,
        request: Request<ChangeMembershipRequest>,
    ) -> Result<Response<ChangeMembershipResponse>, Status> {
        let started = Instant::now();
        let req = request.into_inner();
        let result = async {
            if req.voter_ids.is_empty() {
                return Err(Status::invalid_argument("voter_ids must not be empty"));
            }
            let voters: BTreeSet<u64> = req.voter_ids.into_iter().collect();
            self.inner
                .change_membership(voters, req.retain_learners)
                .await?;
            Ok(Response::new(ChangeMembershipResponse {}))
        }
        .await;
        self.inner.metrics.record_rpc(
            Method::ChangeMembership,
            result_code(&result),
            started.elapsed(),
        );
        result
    }

    async fn transfer_leader(
        &self,
        request: Request<TransferLeaderRequest>,
    ) -> Result<Response<TransferLeaderResponse>, Status> {
        let started = Instant::now();
        let req = request.into_inner();
        let result = async {
            if req.target_node_id == 0 {
                return Err(Status::invalid_argument("target_node_id must be non-zero"));
            }
            self.inner.transfer_leader(req.target_node_id).await?;
            Ok(Response::new(TransferLeaderResponse {}))
        }
        .await;
        self.inner.metrics.record_rpc(
            Method::TransferLeader,
            result_code(&result),
            started.elapsed(),
        );
        result
    }
}
