//! The peppydb server (plans 0026, 0031, 0032): the `peppydb.kv.v1` and
//! `peppydb.raft.v1` gRPC services over [`server_raft::RaftNode`] and an
//! external-mode [`engine::Db`].
//!
//! [`serve`] is the entry point. Every server runs Raft (roadmap decision 10);
//! a single node is a 1-node group. Writes propose through Raft, reads pass a
//! linearizable read barrier (`ReadIndex`) and read the local engine on the
//! blocking pool, and non-leaders answer `UNAVAILABLE` with a `LeaderHint` in
//! status details.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kv_proto::kv::v1::admin_service_server::AdminServiceServer;
use kv_proto::kv::v1::kv_service_server::KvServiceServer;
use raft_proto::raft::v1::raft_service_server::RaftServiceServer;
use server_raft::{
    Command, DbStateMachine, DiskLog, LogOptions, NodeId, Op, PeerInfo, RaftConfig, RaftNode, Role,
    StartError,
};
use tokio::net::TcpListener;
use tonic::server::NamedService;
use tonic::transport::server::TcpIncoming;
use tonic_health::ServingStatus;

mod admin;
mod bridge;
pub mod cli;
mod keys;
mod kv;
mod metrics;
mod raft_transport;
mod status;

#[cfg(test)]
mod tests;

pub use keys::FormatError;

/// The server's version, as Cargo reports it; Buck2 builds report
/// `unknown`.
pub const VERSION: &str = match option_env!("CARGO_PKG_VERSION") {
    Some(version) => version,
    None => "unknown",
};

/// Server limits.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Engine calls that may run at once; more requests wait. Default 1024.
    pub max_inflight: usize,
    /// The largest request message, in bytes. Default 4 MiB.
    pub max_request_bytes: usize,
    /// The most records in one scan page. Default 1000.
    pub scan_max_records: usize,
    /// A scan page stops once its keys and values reach this many bytes.
    /// Default 1 MiB.
    pub scan_max_bytes: usize,
    /// Size of each `AdminService.Snapshot` chunk, in bytes. Default 256 KiB.
    pub snapshot_chunk_bytes: usize,
    /// Maximum `AdminService.Snapshot` streams that may run at once.
    /// Default 4.
    pub max_concurrent_snapshots: usize,
    /// How long shutdown waits for in-flight requests to finish. Default
    /// 10 s.
    pub shutdown_grace: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_inflight: 1024,
            max_request_bytes: 4 << 20,
            scan_max_records: 1000,
            scan_max_bytes: 1 << 20,
            snapshot_chunk_bytes: 256 << 10,
            max_concurrent_snapshots: 4,
            shutdown_grace: Duration::from_secs(10),
        }
    }
}

impl Config {
    fn validate(&self) -> Result<(), ServeError> {
        if self.max_inflight == 0 || self.max_inflight > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(ServeError::Config(
                "max_inflight must be in 1..=Semaphore::MAX_PERMITS",
            ));
        }
        if u32::try_from(self.max_inflight).is_err() {
            return Err(ServeError::Config("max_inflight must fit in a u32"));
        }
        if self.scan_max_records == 0 || self.scan_max_bytes == 0 {
            return Err(ServeError::Config("scan limits must be positive"));
        }
        if self.snapshot_chunk_bytes == 0 {
            return Err(ServeError::Config("snapshot_chunk_bytes must be positive"));
        }
        if self.max_concurrent_snapshots == 0
            || self.max_concurrent_snapshots > tokio::sync::Semaphore::MAX_PERMITS
        {
            return Err(ServeError::Config(
                "max_concurrent_snapshots must be in 1..=Semaphore::MAX_PERMITS",
            ));
        }
        Ok(())
    }
}

/// Configuration for one Raft-replicated server node.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub node_id: NodeId,
    pub data_dir: PathBuf,
    pub peers: BTreeMap<NodeId, PeerInfo>,
    /// Initialize a brand-new cluster when this node starts empty. Ignored on
    /// restart. When false and empty, the node waits uninitialized to join an
    /// existing cluster via `AdminService.AddLearner`.
    pub bootstrap: bool,
    pub direct_io: bool,
    pub log: LogOptions,
    pub raft: RaftConfig,
    pub server: Config,
}

/// How [`serve`] ended.
#[derive(Debug)]
pub enum Exit {
    /// The shutdown future completed cleanly.
    Clean,
    /// A fatal Raft or storage failure stopped the server.
    Fatal(String),
}

/// Why [`serve`] failed.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("invalid server configuration: {0}")]
    Config(&'static str),
    #[error("invalid peer configuration: {0}")]
    Peers(String),
    #[error(transparent)]
    Format(#[from] FormatError),
    #[error("storage error: {0}")]
    Storage(#[from] std::io::Error),
    #[error("raft startup error: {0}")]
    Raft(#[from] StartError),
    #[error("transport error: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("a blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Serve a Raft-replicated peppydb node on `listener` (and optional HTTP
/// `metrics_listener`) until `shutdown` completes or a fatal storage error
/// stops the node.
pub async fn serve(
    node_config: NodeConfig,
    listener: TcpListener,
    metrics_listener: Option<TcpListener>,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<Exit, ServeError> {
    let (inner, has_format, should_bootstrap) = prepare(node_config).await?;
    run(
        inner,
        has_format,
        should_bootstrap,
        listener,
        metrics_listener,
        shutdown,
    )
    .await
}

pub(crate) async fn prepare(
    node_config: NodeConfig,
) -> Result<(Arc<bridge::Inner>, bool, bool), ServeError> {
    node_config.server.validate()?;
    if !node_config.peers.contains_key(&node_config.node_id) {
        return Err(ServeError::Peers(format!(
            "peers map does not contain this node's id {}",
            node_config.node_id
        )));
    }
    let data_dir = node_config.data_dir.clone();
    let log_options = node_config.log;
    let direct_io = node_config.direct_io;
    let (log, sm, has_format, limits) = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&data_dir)?;
        let log = DiskLog::open_real(&data_dir.join("raft-log"), log_options)?;
        let sync = log.sync_hook();
        let sm = DbStateMachine::open_dir(&data_dir.join("db"), direct_io, sync)?;
        let db = sm.db()?;
        let has_format = keys::check_format_readonly(&db).map_err(|e| match e {
            FormatError::Engine(engine::Error::Io { source, .. }) => ServeError::Storage(source),
            other => ServeError::Format(other),
        })?;
        let limits = sm.limits()?;
        Ok::<_, ServeError>((log, sm, has_format, limits))
    })
    .await??;

    let transport = Arc::new(
        raft_transport::GrpcTransport::new(&node_config.peers).map_err(ServeError::Peers)?,
    );
    let node = Arc::new(
        RaftNode::start(
            node_config.node_id,
            &node_config.raft,
            log,
            sm.clone(),
            transport,
        )
        .await?,
    );
    let should_bootstrap = node_config.bootstrap;
    let inner = Arc::new(bridge::Inner::new(
        node,
        sm,
        node_config.peers,
        limits,
        node_config.server,
    ));
    Ok((inner, has_format, should_bootstrap))
}

pub(crate) async fn run(
    inner: Arc<bridge::Inner>,
    has_format: bool,
    should_bootstrap: bool,
    listener: TcpListener,
    metrics_listener: Option<TcpListener>,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<Exit, ServeError> {
    let config = inner.config;
    let (bg_stop_tx, bg_stop_rx) = tokio::sync::watch::channel(false);
    let metrics_task = metrics_listener.map(|ml| {
        tokio::spawn(metrics::serve_http(
            ml,
            Arc::clone(&inner),
            bg_stop_rx.clone(),
        ))
    });

    let kv = KvServiceServer::new(kv::Kv {
        inner: Arc::clone(&inner),
    })
    .max_decoding_message_size(config.max_request_bytes);
    let admin = AdminServiceServer::new(admin::Admin {
        inner: Arc::clone(&inner),
    })
    .max_decoding_message_size(config.max_request_bytes);
    let raft_svc = RaftServiceServer::new(raft_transport::RaftRpcServer {
        node: Arc::clone(&inner.node),
    });

    let (health, health_service) = tonic_health::server::health_reporter();
    let names = [
        "",
        <KvServiceServer<kv::Kv> as NamedService>::NAME,
        <AdminServiceServer<admin::Admin> as NamedService>::NAME,
    ];
    for name in names {
        health
            .set_service_status(name, ServingStatus::Serving)
            .await;
    }

    // Bootstrap (when requested and empty) and write the format marker once a
    // leader is elected, in the background so tonic starts serving peer RPCs
    // immediately.
    let bootstrap_task = {
        let inner = Arc::clone(&inner);
        let mut stop_rx = bg_stop_rx.clone();
        tokio::spawn(async move {
            if should_bootstrap {
                let st = inner.raft_status();
                if st.last_log_index.is_none() && st.applied.is_none() && st.snapshot.is_none() {
                    let voters = inner_peers(&inner);
                    if let Err(err) = inner.node.initialize(voters).await {
                        tracing::debug!(%err, "cluster initialize skipped or already initialized");
                    }
                }
            }
            if !has_format {
                loop {
                    if *stop_rx.borrow() {
                        break;
                    }
                    if let Ok(db) = inner.sm.db()
                        && db.get(keys::FORMAT_KEY).ok().flatten().is_some()
                    {
                        break;
                    }
                    if inner.raft_status().role == Role::Leader {
                        let cmd = Command {
                            ops: vec![Op::Put {
                                key: keys::FORMAT_KEY.to_vec(),
                                value: keys::FORMAT_VERSION.to_vec(),
                            }],
                        };
                        if inner.node.propose(cmd).await.is_ok() {
                            break;
                        }
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                        _ = stop_rx.changed() => break,
                    }
                }
            }
        })
    };

    let (stopping, stopped) = tokio::sync::oneshot::channel();
    let signal = {
        let inner = Arc::clone(&inner);
        let mut health = health.clone();
        async move {
            tokio::select! {
                () = shutdown => tracing::info!("shutting down"),
                () = inner.fatal_notify.notified() => {}
            }
            for name in names {
                health
                    .set_service_status(name, ServingStatus::NotServing)
                    .await;
                health.clear_service_status(name).await;
            }
            let _ = stopping.send(());
        }
    };
    let server = tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(kv)
        .add_service(admin)
        .add_service(raft_svc)
        .serve_with_incoming_shutdown(incoming(listener), signal);
    tokio::select! {
        result = server => result?,
        () = async {
            let _ = stopped.await;
            tokio::time::sleep(config.shutdown_grace).await;
        } => tracing::warn!("shutdown grace period expired with requests in flight"),
    }

    let _ = bg_stop_tx.send(true);
    let _ = bootstrap_task.await;
    if let Some(task) = metrics_task {
        let _ = task.await;
    }

    if let Some(error) = inner.take_fatal() {
        inner.drain_and_shutdown().await;
        return Ok(Exit::Fatal(error));
    }
    // Best-effort checkpoint on clean shutdown so restart replays less log.
    let sm = inner.sm.clone();
    let _ = tokio::task::spawn_blocking(move || sm.checkpoint_now()).await;
    inner.drain_and_shutdown().await;
    Ok(Exit::Clean)
}

/// The accepted connections of `listener`, with `TCP_NODELAY` set.
///
/// A gRPC response is several small writes (HEADERS, DATA, trailers). With
/// Nagle's algorithm on, a later write waits for the ACK of an earlier one,
/// and the client delays that ACK by up to 40 ms on Linux, so one request
/// at a time completes about every 40 ms. `TcpIncoming::from` leaves the
/// option unset, and `Server::tcp_nodelay` applies only to `Server::serve`.
pub(crate) fn incoming(listener: TcpListener) -> TcpIncoming {
    TcpIncoming::from(listener).with_nodelay(Some(true))
}

fn inner_peers(inner: &bridge::Inner) -> BTreeMap<NodeId, PeerInfo> {
    inner.peers_map()
}
