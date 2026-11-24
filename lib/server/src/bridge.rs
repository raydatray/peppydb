//! The Raft and blocking-read bridge (plans 0026, 0032).
//!
//! Writes propose through [`server_raft::RaftNode`] without holding a
//! blocking-pool thread. Reads await [`RaftNode::read_barrier`] and then read
//! the latest `engine::Db` from [`DbStateMachine`] on tokio's blocking pool
//! under the `max_inflight` semaphore.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use engine::Db;
use kv_proto::kv::v1::LeaderHint as PbLeaderHint;
use prost::Message;
use server_raft::{
    Command, DbStateMachine, LeaderHint, MembershipError, NodeId, Op, PeerInfo, ProposeError,
    RaftNode, ReadError, Role, Status as RaftStatus,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tonic::{Code, Status};

use crate::Config;
use crate::metrics::Metrics;

/// State shared by the gRPC services and the metrics endpoint.
pub(crate) struct Inner {
    pub(crate) node: Arc<RaftNode<DbStateMachine>>,
    pub(crate) sm: DbStateMachine,
    peers: BTreeMap<NodeId, PeerInfo>,
    pub(crate) max_key_len: usize,
    pub(crate) max_value_len: usize,
    inflight: Arc<Semaphore>,
    snapshots: Arc<Semaphore>,
    pub(crate) config: Config,
    pub(crate) metrics: Metrics,
    /// First fatal Raft/storage error: triggers fail-stop shutdown.
    fatal: Mutex<Option<String>>,
    /// Woken once on the first fatal error.
    pub(crate) fatal_notify: Notify,
}

impl Inner {
    pub(crate) fn new(
        node: Arc<RaftNode<DbStateMachine>>,
        sm: DbStateMachine,
        peers: BTreeMap<NodeId, PeerInfo>,
        limits: (usize, usize),
        config: Config,
    ) -> Self {
        Self {
            node,
            sm,
            peers,
            max_key_len: limits.0,
            max_value_len: limits.1,
            inflight: Arc::new(Semaphore::new(config.max_inflight)),
            snapshots: Arc::new(Semaphore::new(config.max_concurrent_snapshots)),
            config,
            metrics: Metrics::new(),
            fatal: Mutex::new(None),
            fatal_notify: Notify::new(),
        }
    }

    /// Render the current Prometheus metrics, reading `applied_seq` if the
    /// database is still open.
    pub(crate) fn render_metrics(&self) -> String {
        let applied_seq = self.sm.db().ok().map(|db| db.applied_seq());
        let raft = self.node.status();
        self.metrics.render(applied_seq, Some(&raft))
    }

    pub(crate) fn raft_status(&self) -> RaftStatus {
        self.node.status()
    }

    pub(crate) fn leader_addr(&self, id: NodeId) -> Option<String> {
        let status = self.node.status();
        status
            .membership
            .nodes
            .get(&id)
            .filter(|p| !p.addr.is_empty())
            .map(|p| p.addr.clone())
            .or_else(|| self.peers.get(&id).map(|p| p.addr.clone()))
    }

    pub(crate) fn peers_map(&self) -> BTreeMap<NodeId, PeerInfo> {
        self.peers.clone()
    }

    /// Add a non-voting learner to the Raft group (non-blocking).
    pub(crate) async fn add_learner(&self, id: NodeId, addr: String) -> Result<(), Status> {
        self.node
            .add_learner(id, PeerInfo { addr })
            .await
            .map(|_| ())
            .map_err(|err| self.membership_status(err))
    }

    /// Change the voting set via openraft joint consensus.
    pub(crate) async fn change_membership(
        &self,
        voters: BTreeSet<NodeId>,
        retain_learners: bool,
    ) -> Result<(), Status> {
        self.node
            .change_membership(voters, retain_learners)
            .await
            .map(|_| ())
            .map_err(|err| self.membership_status(err))
    }

    /// Ask the leader to transfer leadership to `target`.
    pub(crate) async fn transfer_leader(&self, target: NodeId) -> Result<(), Status> {
        let status = self.node.status();
        if status.role != Role::Leader {
            let hint = LeaderHint {
                id: status.leader,
                node: status
                    .leader
                    .and_then(|id| status.membership.nodes.get(&id).cloned()),
            };
            return Err(self.not_leader_status(hint));
        }
        let is_voter = status
            .membership
            .configs
            .iter()
            .any(|cfg| cfg.contains(&target));
        if !is_voter {
            return Err(Status::invalid_argument(format!(
                "target node {target} is not a current voter"
            )));
        }
        self.node.transfer_leader(target).await.map_err(|err| {
            self.record_fatal(err.to_string());
            Status::unavailable(err.to_string())
        })
    }

    /// Propose a replicated write through Raft.
    pub(crate) async fn propose(&self, ops: Vec<Op>) -> Result<(), Status> {
        self.node
            .propose(Command { ops })
            .await
            .map(|_| ())
            .map_err(|err| self.propose_status(err))
    }

    /// Confirm leadership and wait for local apply through the read point.
    pub(crate) async fn read_barrier(&self) -> Result<(), Status> {
        self.node
            .read_barrier()
            .await
            .map(|_| ())
            .map_err(|err| self.read_status(err))
    }

    /// Commit a checkpoint with the Raft log sync hook and trigger log purge.
    pub(crate) async fn checkpoint(self: &Arc<Self>) -> Result<engine::Checkpoint, Status> {
        let sm = self.sm.clone();
        let ckpt = self
            .run_blocking(move || sm.checkpoint_now())
            .await?
            .map_err(|e| self.engine_status(e))?;
        // Best-effort openraft snapshot trigger so the Raft log can purge.
        if let Err(err) = self.node.snapshot().await {
            self.record_fatal(err.to_string());
        }
        Ok(ckpt)
    }

    /// Run `f` on the blocking pool against the state machine's current `Db`,
    /// holding one in-flight permit until `f` returns.
    pub(crate) async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Db) -> Result<T, engine::Error> + Send + 'static,
    ) -> Result<T, Status> {
        let sm = self.sm.clone();
        self.run_blocking(move || {
            let db = sm
                .db()
                .map_err(|source| engine::Error::Io { op: "db", source })?;
            f(&db)
        })
        .await?
        .map_err(|error| self.engine_status(error))
    }

    /// Acquire a snapshot permit and an in-flight permit, then run `f` on the
    /// blocking pool without awaiting it.
    pub(crate) async fn spawn_snapshot(
        self: &Arc<Self>,
        f: impl FnOnce(&DbStateMachine) + Send + 'static,
    ) -> Result<(), Status> {
        let snapshot_permit = Arc::clone(&self.snapshots)
            .acquire_owned()
            .await
            .map_err(|_| shutting_down())?;
        let inflight_permit = Arc::clone(&self.inflight)
            .acquire_owned()
            .await
            .map_err(|_| shutting_down())?;
        let sm = self.sm.clone();
        let inner = Arc::clone(self);
        inner.metrics.inc_inflight();
        inner.metrics.inc_snapshot_active();
        tokio::task::spawn_blocking(move || {
            let _inflight: OwnedSemaphorePermit = inflight_permit;
            let _snapshot: OwnedSemaphorePermit = snapshot_permit;
            let _guard = SnapshotGuard(inner);
            f(&sm);
        });
        Ok(())
    }

    pub(crate) fn engine_status(&self, error: engine::Error) -> Status {
        let status = crate::status::status(&error);
        if matches!(error, engine::Error::Wal(_)) {
            self.record_fatal(error.to_string());
        }
        status
    }

    fn propose_status(&self, error: ProposeError) -> Status {
        match error {
            ProposeError::NotLeader(hint) => self.not_leader_status(hint),
            ProposeError::OutcomeUnknown => Status::unavailable("write outcome unknown; retry"),
            ProposeError::Stopped(msg) => {
                self.record_fatal(msg.clone());
                Status::unavailable(msg)
            }
            ProposeError::Internal(msg) => {
                tracing::error!(%msg, "raft propose error");
                Status::internal(msg)
            }
        }
    }

    fn read_status(&self, error: ReadError) -> Status {
        match error {
            ReadError::NotLeader(hint) => self.not_leader_status(hint),
            ReadError::NoQuorum => Status::unavailable("no quorum for linearizable read"),
            ReadError::Stopped(msg) => {
                self.record_fatal(msg.clone());
                Status::unavailable(msg)
            }
        }
    }

    fn membership_status(&self, error: MembershipError) -> Status {
        match error {
            MembershipError::NotLeader(hint) => self.not_leader_status(hint),
            MembershipError::InProgress => Status::aborted("membership change already in progress"),
            MembershipError::Invalid(msg) => Status::invalid_argument(msg),
            MembershipError::OutcomeUnknown => {
                Status::unavailable("membership change outcome unknown; retry")
            }
            MembershipError::Stopped(msg) => {
                self.record_fatal(msg.clone());
                Status::unavailable(msg)
            }
        }
    }

    fn not_leader_status(&self, hint: LeaderHint) -> Status {
        let leader_id = hint.id;
        let leader_addr = hint
            .node
            .map(|n| n.addr)
            .or_else(|| leader_id.and_then(|id| self.leader_addr(id)));
        match (leader_id, leader_addr) {
            (Some(id), Some(addr)) => encode_leader_hint_status(id, addr),
            _ => Status::unavailable("not leader; no leader elected"),
        }
    }

    pub(crate) fn record_fatal(&self, message: String) {
        let mut fatal = self.fatal.lock().unwrap_or_else(PoisonError::into_inner);
        if fatal.is_none() {
            tracing::error!(%message, "fatal Raft/storage error; stopping the server");
            *fatal = Some(message);
            self.fatal_notify.notify_one();
        }
    }

    pub(crate) fn take_fatal(&self) -> Option<String> {
        self.fatal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    async fn run_blocking<R: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce() -> R + Send + 'static,
    ) -> Result<R, Status> {
        let permit = Arc::clone(&self.inflight)
            .acquire_owned()
            .await
            .map_err(|_| shutting_down())?;
        let inner = Arc::clone(self);
        inner.metrics.inc_inflight();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _guard = InflightGuard(inner);
            f()
        })
        .await
        .map_err(|error| Status::internal(format!("engine task failed: {error}")))
    }

    /// Stop new blocking calls, wait for running ones, and shut down Raft.
    pub(crate) async fn drain_and_shutdown(&self) {
        let all = u32::try_from(self.config.max_inflight).unwrap_or(u32::MAX);
        let permits = self.inflight.acquire_many(all).await.ok();
        self.inflight.close();
        self.snapshots.close();
        drop(permits);
        self.node.shutdown().await;
    }
}

pub(crate) fn role_str(role: Role) -> &'static str {
    match role {
        Role::Leader => "leader",
        Role::Candidate => "candidate",
        Role::Follower => "follower",
        Role::Learner => "learner",
        Role::Stopped => "shutdown",
    }
}

struct InflightGuard(Arc<Inner>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.metrics.dec_inflight();
    }
}

struct SnapshotGuard(Arc<Inner>);

impl Drop for SnapshotGuard {
    fn drop(&mut self) {
        self.0.metrics.dec_snapshot_active();
        self.0.metrics.dec_inflight();
    }
}

fn shutting_down() -> Status {
    Status::unavailable("the server is shutting down")
}

const LEADER_HINT_TYPE_URL: &str = "type.googleapis.com/peppydb.kv.v1.LeaderHint";

/// Wire-compatible `google.protobuf.Any` for gRPC rich status details.
#[derive(prost::Message)]
struct ProtoAny {
    #[prost(string, tag = "1")]
    type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
}

/// Wire-compatible `google.rpc.Status` required in `grpc-status-details-bin`.
#[derive(prost::Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<ProtoAny>,
}

fn encode_leader_hint_status(id: NodeId, addr: String) -> Status {
    let message = format!("not leader; leader is node {id} at {addr}");
    let pb = PbLeaderHint {
        leader_id: id,
        leader_addr: addr,
    };
    let rpc_status = RpcStatus {
        code: Code::Unavailable as i32,
        message: message.clone(),
        details: vec![ProtoAny {
            type_url: LEADER_HINT_TYPE_URL.to_owned(),
            value: pb.encode_to_vec(),
        }],
    };
    Status::with_details(
        Code::Unavailable,
        message,
        rpc_status.encode_to_vec().into(),
    )
}

#[cfg(test)]
pub(crate) fn decode_leader_hint(status: &Status) -> Option<PbLeaderHint> {
    let rpc_status = RpcStatus::decode(status.details()).ok()?;
    if rpc_status.code != status.code() as i32 || rpc_status.message != status.message() {
        return None;
    }
    rpc_status
        .details
        .into_iter()
        .find(|any| any.type_url == LEADER_HINT_TYPE_URL)
        .and_then(|any| PbLeaderHint::decode(any.value.as_slice()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leader_hint_encodes_as_google_rpc_status() {
        let status = encode_leader_hint_status(2, "127.0.0.1:7421".to_owned());
        assert_eq!(status.code(), Code::Unavailable);
        let hint = decode_leader_hint(&status).expect("decode LeaderHint from google.rpc.Status");
        assert_eq!(hint.leader_id, 2);
        assert_eq!(hint.leader_addr, "127.0.0.1:7421");
    }
}
