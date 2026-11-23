//! The Raft and blocking-read bridge (plans 0026, 0032).
//!
//! Writes propose through [`server_raft::RaftNode`] without holding a
//! blocking-pool thread. Reads await [`RaftNode::read_barrier`] and then read
//! the latest `engine::Db` from [`DbStateMachine`] on tokio's blocking pool
//! under the `max_inflight` semaphore.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use engine::Db;
use kv_proto::kv::v1::LeaderHint as PbLeaderHint;
use prost::Message;
use server_raft::{
    Command, DbStateMachine, LeaderHint, NodeId, Op, PeerInfo, ProposeError, RaftNode, ReadError,
    Role, Status as RaftStatus,
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
        self.metrics.render(applied_seq)
    }

    pub(crate) fn raft_status(&self) -> RaftStatus {
        self.node.status()
    }

    pub(crate) fn leader_addr(&self, id: NodeId) -> Option<String> {
        self.peers.get(&id).map(|p| p.addr.clone())
    }

    pub(crate) fn peers_map(&self) -> BTreeMap<NodeId, PeerInfo> {
        self.peers.clone()
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

    fn not_leader_status(&self, hint: LeaderHint) -> Status {
        let leader_id = hint.id;
        let leader_addr = hint
            .node
            .map(|n| n.addr)
            .or_else(|| leader_id.and_then(|id| self.leader_addr(id)));
        match (leader_id, leader_addr) {
            (Some(id), Some(addr)) => {
                let pb = PbLeaderHint {
                    leader_id: id,
                    leader_addr: addr,
                };
                Status::with_details(
                    Code::Unavailable,
                    format!("not leader; leader is node {id} at {}", pb.leader_addr),
                    pb.encode_to_vec().into(),
                )
            }
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
