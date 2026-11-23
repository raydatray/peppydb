//! In-process tests: a temporary database, `serve` on `127.0.0.1:0`, and a
//! generated client.

mod admin;
mod kv;
mod process;
mod scan;
mod transport;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kv_proto::kv::v1::admin_service_client::AdminServiceClient;
use kv_proto::kv::v1::kv_service_client::KvServiceClient;
use server_raft::{LogOptions, PeerInfo, RaftConfig, Role};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic_health::pb::health_client::HealthClient;

use crate::bridge::Inner;
use crate::{Config, Exit, NodeConfig, ServeError, prepare, run};

/// A running 1-node Raft server over a data directory.
pub(super) struct Server {
    pub(super) addr: SocketAddr,
    pub(super) metrics_addr: Option<SocketAddr>,
    inner: Arc<Inner>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<Exit, ServeError>>>,
}

impl Server {
    /// Open the database at `path` and serve it with `config` as a 1-node
    /// Raft cluster, waiting until it is elected leader and formatted.
    pub(super) async fn start(path: &Path, config: Config) -> Self {
        Self::start_inner(path, config, false).await
    }

    pub(super) async fn start_with_metrics_listener(path: &Path, config: Config) -> Self {
        Self::start_inner(path, config, true).await
    }

    async fn start_inner(path: &Path, config: Config, enable_metrics: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (metrics_listener, metrics_addr) = if enable_metrics {
            let ml = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let ma = ml.local_addr().unwrap();
            (Some(ml), Some(ma))
        } else {
            (None, None)
        };
        let peers = BTreeMap::from([(
            1,
            PeerInfo {
                addr: addr.to_string(),
            },
        )]);
        let node_config = NodeConfig {
            node_id: 1,
            data_dir: PathBuf::from(path),
            peers,
            bootstrap: true,
            direct_io: false,
            log: LogOptions::default(),
            raft: RaftConfig::default(),
            server: config,
        };
        let (stop, stopped) = oneshot::channel();
        let (inner, has_format, should_bootstrap) = prepare(node_config).await.expect("prepare");
        let task = tokio::spawn(run(
            Arc::clone(&inner),
            has_format,
            should_bootstrap,
            listener,
            metrics_listener,
            async {
                let _ = stopped.await;
            },
        ));
        // Wait until the 1-node cluster elects itself leader and writes the
        // format marker so callers can issue KV RPCs immediately.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if inner.raft_status().role == Role::Leader
                    && inner.node.read_barrier().await.is_ok()
                    && inner
                        .sm
                        .db()
                        .ok()
                        .and_then(|db| db.get(crate::keys::FORMAT_KEY).ok().flatten())
                        .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("single-node cluster elected leader");
        Self {
            addr,
            metrics_addr,
            inner,
            stop: Some(stop),
            task: Some(task),
        }
    }

    pub(super) async fn kv(&self) -> KvServiceClient<Channel> {
        KvServiceClient::connect(format!("http://{}", self.addr))
            .await
            .expect("connect")
    }

    pub(super) async fn admin(&self) -> AdminServiceClient<Channel> {
        AdminServiceClient::connect(format!("http://{}", self.addr))
            .await
            .expect("connect")
    }

    pub(super) async fn health(&self) -> HealthClient<Channel> {
        let channel = Channel::from_shared(format!("http://{}", self.addr))
            .unwrap()
            .connect()
            .await
            .expect("connect");
        HealthClient::new(channel)
    }

    /// Inject a fatal storage/Raft failure to test fail-stop shutdown.
    pub(super) fn trigger_fatal(&self, msg: &str) {
        self.inner.record_fatal(msg.to_owned());
    }

    /// Wait for `serve` to return without triggering shutdown.
    pub(super) async fn exited(mut self) -> Result<Exit, ServeError> {
        self.task.take().unwrap().await.unwrap()
    }

    /// Trigger shutdown and wait for `serve` to return.
    pub(super) async fn stop(mut self) -> Result<Exit, ServeError> {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap()
    }
}

/// Open the external-mode engine database at `path` (usually `<data-dir>/db`).
pub(super) fn open_external_db(path: &Path) -> engine::Db {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match engine::Db::open(
            path,
            engine::Options {
                mode: engine::Mode::External,
                ..engine::Options::default()
            },
        ) {
            Err(engine::Error::Locked) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result.expect("open the database"),
        }
    }
}

/// A small deterministic generator for randomized tests.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub(super) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub(super) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}
