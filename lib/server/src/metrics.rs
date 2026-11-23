//! Server metrics in Prometheus/OpenMetrics text format over HTTP/1.1.

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;
use server_raft::{Role, Status as RaftStatus};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tonic::Code;

use crate::bridge::{Inner, role_str};

/// Latency histogram buckets in seconds: 100 µs up to 10 s.
const LATENCY_BUCKETS: [f64; 16] = [
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
    5.0, 10.0,
];

/// HTTP read/write timeout for a single `/metrics` scrape.
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub(crate) enum Method {
    Get,
    Put,
    Delete,
    Scan,
    Status,
    Checkpoint,
    Snapshot,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Put => "put",
            Self::Delete => "delete",
            Self::Scan => "scan",
            Self::Status => "status",
            Self::Checkpoint => "checkpoint",
            Self::Snapshot => "snapshot",
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RpcLabels {
    method: &'static str,
    code: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct MethodLabels {
    method: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RoleLabels {
    role: &'static str,
}

const ALL_ROLES: [Role; 5] = [
    Role::Leader,
    Role::Follower,
    Role::Candidate,
    Role::Learner,
    Role::Stopped,
];

/// Prometheus metrics for one running server.
pub(crate) struct Metrics {
    registry: Registry,
    started_at: Instant,
    rpc_requests: Family<RpcLabels, Counter>,
    rpc_duration: Family<MethodLabels, Histogram>,
    inflight: Gauge,
    snapshot_active: Gauge,
    snapshot_bytes: Counter,
    applied_seq: Gauge,
    uptime_seconds: Gauge<f64, AtomicU64>,
    raft_term: Gauge,
    raft_role: Family<RoleLabels, Gauge>,
    raft_commit_index: Gauge,
    raft_applied_index: Gauge,
}

impl Metrics {
    pub(crate) fn new() -> Self {
        let mut registry = Registry::with_prefix("peppydb");

        let rpc_requests: Family<RpcLabels, Counter> = Family::default();
        registry.register(
            "rpc_requests",
            "Completed gRPC requests by method and status code",
            rpc_requests.clone(),
        );

        let rpc_duration: Family<MethodLabels, Histogram> =
            Family::new_with_constructor(|| Histogram::new(LATENCY_BUCKETS));
        registry.register(
            "rpc_duration_seconds",
            "End-to-end gRPC handler latency in seconds",
            rpc_duration.clone(),
        );

        let inflight = Gauge::default();
        registry.register(
            "inflight_requests",
            "Engine calls currently holding a bridge permit",
            inflight.clone(),
        );

        let snapshot_active = Gauge::default();
        registry.register(
            "snapshot_active",
            "AdminService.Snapshot streams currently active",
            snapshot_active.clone(),
        );

        let snapshot_bytes = Counter::default();
        registry.register(
            "snapshot_bytes",
            "Total bytes streamed by AdminService.Snapshot",
            snapshot_bytes.clone(),
        );

        let applied_seq = Gauge::default();
        registry.register(
            "engine_applied_seq",
            "Latest applied sequence reported by engine::Db",
            applied_seq.clone(),
        );

        let uptime_seconds: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "process_uptime_seconds",
            "Seconds since the server started serving",
            uptime_seconds.clone(),
        );

        let raft_term = Gauge::default();
        registry.register("raft_term", "Current Raft term", raft_term.clone());

        let raft_role: Family<RoleLabels, Gauge> = Family::default();
        registry.register(
            "raft_role",
            "Active Raft role (1 for current role, 0 for others)",
            raft_role.clone(),
        );

        let raft_commit_index = Gauge::default();
        registry.register(
            "raft_commit_index",
            "Latest Raft committed log index",
            raft_commit_index.clone(),
        );

        let raft_applied_index = Gauge::default();
        registry.register(
            "raft_applied_index",
            "Latest Raft applied log index",
            raft_applied_index.clone(),
        );

        Self {
            registry,
            started_at: Instant::now(),
            rpc_requests,
            rpc_duration,
            inflight,
            snapshot_active,
            snapshot_bytes,
            applied_seq,
            uptime_seconds,
            raft_term,
            raft_role,
            raft_commit_index,
            raft_applied_index,
        }
    }

    pub(crate) fn record_rpc(&self, method: Method, code: Code, elapsed: Duration) {
        let name = method.as_str();
        self.rpc_requests
            .get_or_create(&RpcLabels {
                method: name,
                code: code_str(code),
            })
            .inc();
        self.rpc_duration
            .get_or_create(&MethodLabels { method: name })
            .observe(elapsed.as_secs_f64());
    }

    pub(crate) fn inc_inflight(&self) {
        self.inflight.inc();
    }

    pub(crate) fn dec_inflight(&self) {
        self.inflight.dec();
    }

    pub(crate) fn inc_snapshot_active(&self) {
        self.snapshot_active.inc();
    }

    pub(crate) fn dec_snapshot_active(&self) {
        self.snapshot_active.dec();
    }

    pub(crate) fn add_snapshot_bytes(&self, bytes: u64) {
        self.snapshot_bytes.inc_by(bytes);
    }

    pub(crate) fn render(&self, applied_seq: Option<u64>, raft: Option<&RaftStatus>) -> String {
        self.uptime_seconds
            .set(self.started_at.elapsed().as_secs_f64());
        if let Some(seq) = applied_seq {
            self.applied_seq.set(i64::try_from(seq).unwrap_or(i64::MAX));
        }
        if let Some(status) = raft {
            self.raft_term
                .set(i64::try_from(status.term).unwrap_or(i64::MAX));
            for role in ALL_ROLES {
                let value = i64::from(status.role == role);
                self.raft_role
                    .get_or_create(&RoleLabels {
                        role: role_str(role),
                    })
                    .set(value);
            }
            if let Some(committed) = status.committed {
                self.raft_commit_index
                    .set(i64::try_from(committed.index).unwrap_or(i64::MAX));
            }
            if let Some(applied) = status.applied {
                self.raft_applied_index
                    .set(i64::try_from(applied.index).unwrap_or(i64::MAX));
            }
        }
        let mut out = String::new();
        encode(&mut out, &self.registry).expect("encoding metrics into String never fails");
        out
    }
}

pub(crate) fn result_code<T>(result: &Result<T, tonic::Status>) -> Code {
    match result {
        Ok(_) => Code::Ok,
        Err(status) => status.code(),
    }
}

fn code_str(code: Code) -> &'static str {
    match code {
        Code::Ok => "ok",
        Code::Cancelled => "cancelled",
        Code::Unknown => "unknown",
        Code::InvalidArgument => "invalid_argument",
        Code::DeadlineExceeded => "deadline_exceeded",
        Code::NotFound => "not_found",
        Code::AlreadyExists => "already_exists",
        Code::PermissionDenied => "permission_denied",
        Code::ResourceExhausted => "resource_exhausted",
        Code::FailedPrecondition => "failed_precondition",
        Code::Aborted => "aborted",
        Code::OutOfRange => "out_of_range",
        Code::Unimplemented => "unimplemented",
        Code::Internal => "internal",
        Code::Unavailable => "unavailable",
        Code::DataLoss => "data_loss",
        Code::Unauthenticated => "unauthenticated",
    }
}

/// Serve `GET /metrics` over HTTP/1.1 until `shutdown` becomes `true`.
pub(crate) async fn serve_http(
    listener: TcpListener,
    inner: Arc<Inner>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let accepted = tokio::select! {
            res = listener.accept() => res,
            _ = shutdown.changed() => break,
        };
        let (stream, _) = match accepted {
            Ok(conn) => conn,
            Err(error) => {
                tracing::warn!(%error, "metrics accept failed");
                continue;
            }
        };
        let inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let _ = tokio::time::timeout(SCRAPE_TIMEOUT, handle_connection(stream, inner)).await;
        });
    }
}

async fn handle_connection(stream: TcpStream, inner: Arc<Inner>) -> io::Result<()> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    while !buf.windows(2).any(|w| w == b"\r\n") && buf.len() < 4096 {
        stream.readable().await?;
        match stream.try_read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
    let request_line = std::str::from_utf8(&buf)
        .ok()
        .and_then(|s| s.lines().next())
        .unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let path = target.split('?').next().unwrap_or("");

    let response = if method == "GET" && path == "/metrics" {
        let body = inner.render_metrics();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/openmetrics-text; version=1.0.0; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    } else {
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
    };

    write_all(&stream, response.as_bytes()).await
}

async fn write_all(stream: &TcpStream, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        stream.writable().await?;
        match stream.try_write(data) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "short write")),
            Ok(n) => data = &data[n..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_render_prometheus_exposition() {
        let metrics = Metrics::new();
        metrics.record_rpc(Method::Put, Code::Ok, Duration::from_millis(2));
        metrics.record_rpc(
            Method::Get,
            Code::InvalidArgument,
            Duration::from_micros(150),
        );
        metrics.inc_inflight();
        metrics.add_snapshot_bytes(4096);

        let text = metrics.render(Some(42), None);
        assert!(text.contains("peppydb_rpc_requests_total{method=\"put\",code=\"ok\"} 1"));
        assert!(
            text.contains("peppydb_rpc_requests_total{method=\"get\",code=\"invalid_argument\"} 1")
        );
        assert!(text.contains("peppydb_rpc_duration_seconds_count{method=\"put\"} 1"));
        assert!(text.contains("peppydb_inflight_requests 1"));
        assert!(text.contains("peppydb_snapshot_bytes_total 4096"));
        assert!(text.contains("peppydb_engine_applied_seq 42"));
        assert!(text.contains("# EOF"));
    }
}
