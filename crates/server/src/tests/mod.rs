//! In-process tests: a temporary database, `serve` on `127.0.0.1:0`, and a
//! generated client.

mod admin;
mod kv;
mod process;
mod scan;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use server_proto::kv::v1::admin_service_client::AdminServiceClient;
use server_proto::kv::v1::kv_service_client::KvServiceClient;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic_health::pb::health_client::HealthClient;

use crate::bridge::Inner;
use crate::{Config, Exit, ServeError, prepare, run};

/// A running server over a database directory.
pub(super) struct Server {
    pub(super) addr: SocketAddr,
    inner: Arc<Inner>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<Exit, ServeError>>>,
}

impl Server {
    /// Open the database at `path` and serve it with `config`.
    pub(super) async fn start(path: &Path, config: Config) -> Self {
        let path = PathBuf::from(path);
        let db = tokio::task::spawn_blocking(move || open_db(&path))
            .await
            .unwrap();
        Self::start_with(db, config).await
    }

    pub(super) async fn start_with(db: engine::Db, config: Config) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        // The listener is already bound, so clients can connect before
        // `serve` starts accepting.
        let inner = prepare(db, config).await.expect("prepare");
        let task = tokio::spawn(run(Arc::clone(&inner), listener, async {
            let _ = stopped.await;
        }));
        Self {
            addr,
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

    /// Make the WAL's next sync fail.
    pub(super) async fn fail_next_wal_sync(&self) {
        self.inner
            .blocking(|db| {
                db.fail_next_wal_sync();
                Ok(())
            })
            .await
            .unwrap();
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

/// Open the database at `path` with default options.
///
/// Retries `Locked` for up to 5 s. The process tests fork children from
/// this test process, and a child holds copies of every open file until it
/// execs, including another test's `LOCK` file: for that moment the lock
/// outlives the database that took it. A real double open still fails.
pub(super) fn open_db(path: &Path) -> engine::Db {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match engine::Db::open(path, engine::Options::default()) {
            Err(engine::Error::Locked) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
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
