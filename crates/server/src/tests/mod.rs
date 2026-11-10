//! In-process tests: a temporary database, `serve` on `127.0.0.1:0`, and a
//! generated client.

mod kv;
mod scan;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use server_proto::kv::v1::kv_service_client::KvServiceClient;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;

use crate::{Config, Exit, ServeError, serve};

/// A running server over a database directory.
pub(super) struct Server {
    pub(super) addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<Exit, ServeError>>>,
}

impl Server {
    /// Open the database at `path` and serve it with `config`.
    pub(super) async fn start(path: &Path, config: Config) -> Self {
        let path = PathBuf::from(path);
        let db =
            tokio::task::spawn_blocking(move || engine::Db::open(path, engine::Options::default()))
                .await
                .unwrap()
                .expect("open the database");
        Self::start_with(db, config).await
    }

    pub(super) async fn start_with(db: engine::Db, config: Config) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        // The listener is already bound, so clients can connect before
        // `serve` starts accepting.
        let task = tokio::spawn(serve(db, listener, config, async {
            let _ = stopped.await;
        }));
        Self {
            addr,
            stop: Some(stop),
            task: Some(task),
        }
    }

    pub(super) async fn kv(&self) -> KvServiceClient<Channel> {
        KvServiceClient::connect(format!("http://{}", self.addr))
            .await
            .expect("connect")
    }

    /// Trigger shutdown and wait for `serve` to return.
    pub(super) async fn stop(mut self) -> Result<Exit, ServeError> {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap()
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
