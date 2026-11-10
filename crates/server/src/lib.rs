//! The peppydb server (plan 0026): the `peppydb.kv.v1` gRPC services over
//! one [`engine::Db`].
//!
//! [`serve`] is the entry point. It takes an opened database and a bound
//! listener, serves until its shutdown future completes, then drains
//! requests and closes the database. A WAL failure stops it too
//! ([`Exit::Fatal`]): the engine refuses writes until a reopen, so the
//! process should exit and be restarted.
//!
//! - **Semantics are the engine's.** A `Put` or `Delete` returns `OK` only
//!   once durable; single keys are linearizable; scans are consistent per
//!   leaf ([`engine`], "Consistency").
//! - **Errors are gRPC statuses**, mapped from [`engine::Error`]: limits
//!   are `INVALID_ARGUMENT`, a WAL failure is `UNAVAILABLE`, and store
//!   errors are `INTERNAL`.
//! - **Stored keys carry a namespace byte**: user keys are stored as
//!   `0x01 ‖ key`; `0x00` is the server's system keyspace.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use engine::Db;
use server_proto::kv::v1::admin_service_server::AdminServiceServer;
use server_proto::kv::v1::kv_service_server::KvServiceServer;
use tokio::net::TcpListener;
use tonic::server::NamedService;
use tonic::transport::server::TcpIncoming;
use tonic_health::ServingStatus;

mod admin;
mod bridge;
pub mod cli;
mod keys;
mod kv;
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
        Ok(())
    }
}

/// How [`serve`] ended.
#[derive(Debug)]
pub enum Exit {
    /// The shutdown future completed; the database closed with a
    /// checkpoint.
    Clean,
    /// A WAL failure stopped the server. The database was dropped without
    /// a checkpoint; a restart reopens it and replays the durable WAL.
    Fatal(engine::Error),
}

/// Why [`serve`] failed.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("invalid server configuration: {0}")]
    Config(&'static str),
    #[error(transparent)]
    Format(#[from] FormatError),
    #[error("transport error: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("closing the database failed: {0}")]
    Close(#[source] engine::Error),
    #[error("a blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Serve `db` on `listener` until `shutdown` completes or the WAL fails,
/// then drain requests and close the database.
///
/// On an empty database `serve` writes the format marker; a database with
/// data but no marker is refused with [`FormatError::Missing`].
///
/// Shutdown marks every service `NOT_SERVING` in the health service, stops
/// accepting connections, waits up to [`Config::shutdown_grace`] for
/// in-flight requests, then waits for every engine call to return. On a
/// clean shutdown it closes the database, which checkpoints; after a WAL
/// failure it drops it.
pub async fn serve(
    db: Db,
    listener: TcpListener,
    config: Config,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<Exit, ServeError> {
    let inner = prepare(db, config).await?;
    run(inner, listener, shutdown).await
}

/// Check the configuration and the format marker.
pub(crate) async fn prepare(db: Db, config: Config) -> Result<Arc<bridge::Inner>, ServeError> {
    config.validate()?;
    let db = tokio::task::spawn_blocking(move || keys::check_format(&db).map(|()| db)).await??;
    Ok(Arc::new(bridge::Inner::new(db, config)))
}

/// Serve, drain, and close.
pub(crate) async fn run(
    inner: Arc<bridge::Inner>,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<Exit, ServeError> {
    let config = inner.config;
    let kv = KvServiceServer::new(kv::Kv {
        inner: Arc::clone(&inner),
    })
    .max_decoding_message_size(config.max_request_bytes);
    let admin = AdminServiceServer::new(admin::Admin {
        inner: Arc::clone(&inner),
    })
    .max_decoding_message_size(config.max_request_bytes);

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

    let (stopping, stopped) = tokio::sync::oneshot::channel();
    let signal = {
        let inner = Arc::clone(&inner);
        let mut health = health.clone();
        async move {
            tokio::select! {
                () = shutdown => tracing::info!("shutting down"),
                () = inner.fatal_notify.notified() => {}
            }
            // Watchers see NOT_SERVING; clearing the statuses then ends
            // their streams, so they do not hold up the drain.
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
        .serve_with_incoming_shutdown(TcpIncoming::from(listener), signal);
    tokio::select! {
        result = server => result?,
        () = async {
            let _ = stopped.await;
            tokio::time::sleep(config.shutdown_grace).await;
        } => tracing::warn!("shutdown grace period expired with requests in flight"),
    }

    let db = inner.take_db().await.expect("only run takes the database");
    if let Some(error) = inner.take_fatal() {
        // A checkpoint cannot commit after a WAL failure: its hook syncs
        // the WAL. The restart replays the durable WAL instead.
        tokio::task::spawn_blocking(move || drop(db)).await?;
        return Ok(Exit::Fatal(error));
    }
    tokio::task::spawn_blocking(move || db.close())
        .await?
        .map_err(ServeError::Close)?;
    Ok(Exit::Clean)
}
