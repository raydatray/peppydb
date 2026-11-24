//! The `peppydb` command line (plan 0026, "The binary").
//!
//! `bin/peppydb` calls [`main`]. The code lives here so that the process
//! tests can run exactly what the binary runs.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use server_raft::{LogOptions, PeerInfo, RaftConfig};

use crate::{Config, Exit, NodeConfig, prepare, run as serve_run};

#[derive(Parser)]
#[command(name = "peppydb", version = crate::VERSION, about = "The peppydb key-value server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve one database over gRPC until SIGINT or SIGTERM.
    Serve(ServeArgs),
}

#[derive(clap::Args)]
struct ServeArgs {
    /// The database directory; created if it does not exist.
    #[arg(long)]
    data_dir: PathBuf,
    /// The address to listen on. There is no TLS or authentication, so the
    /// default is loopback.
    #[arg(long, default_value = "127.0.0.1:7420")]
    listen: SocketAddr,
    /// The WAL flusher waits this long after the first pending write.
    #[arg(long, default_value_t = 0)]
    flush_interval_us: u64,
    /// How the store file is read and written.
    #[arg(long, value_enum, default_value_t = IoMode::Cached)]
    io: IoMode,
    /// Engine calls that may run at once; more requests wait.
    #[arg(long, default_value_t = Config::default().max_inflight)]
    max_inflight: usize,
    /// Optional HTTP address serving Prometheus metrics at `GET /metrics`.
    #[arg(long)]
    metrics_listen: Option<SocketAddr>,
    /// This server's Raft node id.
    #[arg(long, default_value_t = 1)]
    node_id: u64,
    /// Comma-separated initial cluster peers (`id=host:port,...`). When omitted,
    /// only this node is registered locally (`--bootstrap` initializes a 1-node
    /// cluster; omitting `--bootstrap` starts empty awaiting `AddLearner`).
    #[arg(long)]
    peers: Option<String>,
    /// Initialize a brand-new cluster when starting empty. Ignored on restart.
    #[arg(long, default_value_t = false)]
    bootstrap: bool,
    /// Trigger an automatic snapshot after this many log entries.
    #[arg(long)]
    snapshot_after: Option<u64>,
    /// Keep at least this many log entries below the latest snapshot.
    #[arg(long)]
    keep_below_snapshot: Option<u64>,
}

#[derive(Clone, Copy, ValueEnum)]
enum IoMode {
    Cached,
    Direct,
}

/// Run the command line with `args` (including the program name) and
/// return the process exit code: 0 after a clean shutdown, 1 after a WAL
/// failure or a startup error.
pub fn main(args: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> ExitCode {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(2));
        }
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let Command::Serve(args) = cli.command;
    match run(args) {
        Ok(Exit::Clean) => {
            tracing::info!("stopped cleanly");
            ExitCode::SUCCESS
        }
        Ok(Exit::Fatal(error)) => {
            tracing::error!(%error, "stopped after a WAL failure; restart to recover");
            ExitCode::FAILURE
        }
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: ServeArgs) -> Result<Exit, Box<dyn std::error::Error>> {
    let config = Config {
        max_inflight: args.max_inflight,
        ..Config::default()
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Every permitted engine call gets a blocking thread at once.
        .max_blocking_threads(config.max_inflight.saturating_add(64))
        .build()?;
    runtime.block_on(async move {
        let shutdown = shutdown_signal()?;
        let listener = tokio::net::TcpListener::bind(args.listen).await?;
        let bound_addr = listener.local_addr()?;
        let metrics_listener = match args.metrics_listen {
            Some(addr) => {
                let listener = tokio::net::TcpListener::bind(addr).await?;
                tracing::info!("metrics listening on {}", listener.local_addr()?);
                Some(listener)
            }
            None => None,
        };
        let peers = match args.peers.as_deref() {
            Some(spec) => parse_peers(spec)?,
            None => BTreeMap::from([(
                args.node_id,
                PeerInfo {
                    addr: bound_addr.to_string(),
                },
            )]),
        };
        let bootstrap = args.bootstrap;
        let mut raft = RaftConfig::default();
        if let Some(after) = args.snapshot_after {
            raft.snapshot_after = Some(after);
        }
        if let Some(keep) = args.keep_below_snapshot {
            raft.keep_below_snapshot = keep;
        }
        let node_config = NodeConfig {
            node_id: args.node_id,
            data_dir: args.data_dir,
            peers,
            bootstrap,
            direct_io: matches!(args.io, IoMode::Direct),
            log: LogOptions::default(),
            raft,
            server: config,
        };
        let (inner, has_format, should_bootstrap) = prepare(node_config).await?;
        tracing::info!("listening on {bound_addr}");
        Ok(serve_run(
            inner,
            has_format,
            should_bootstrap,
            listener,
            metrics_listener,
            shutdown,
        )
        .await?)
    })
}

/// Completes on the first SIGINT or SIGTERM. A second signal exits at once,
/// which is safe: acknowledged writes are durable. The handlers are
/// installed before this returns, so a signal that arrives before the
/// future is first polled is not lost.
fn shutdown_signal() -> std::io::Result<impl Future<Output = ()> + Send> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => tracing::info!("SIGINT; shutting down"),
            _ = terminate.recv() => tracing::info!("SIGTERM; shutting down"),
        }
        tokio::spawn(async move {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
            tracing::warn!("second signal; exiting without draining");
            std::process::exit(1);
        });
    })
}

fn parse_peers(spec: &str) -> Result<BTreeMap<u64, PeerInfo>, String> {
    let mut peers = BTreeMap::new();
    for item in spec.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (id_str, addr) = item
            .split_once('=')
            .ok_or_else(|| format!("peer entry {item:?} must be `id=addr`"))?;
        let id: u64 = id_str
            .trim()
            .parse()
            .map_err(|_| format!("invalid node id {id_str:?} in --peers"))?;
        let addr = addr.trim();
        if addr.is_empty() {
            return Err(format!("empty address for node {id} in --peers"));
        }
        if peers
            .insert(
                id,
                PeerInfo {
                    addr: addr.to_owned(),
                },
            )
            .is_some()
        {
            return Err(format!("duplicate node id {id} in --peers"));
        }
    }
    if peers.is_empty() {
        return Err("--peers must list at least one peer".to_owned());
    }
    Ok(peers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_peers_accepts_valid_and_rejects_malformed() {
        let parsed = parse_peers("1=127.0.0.1:7420, 2=127.0.0.1:7421, 3=127.0.0.1:7422").unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[&2].addr, "127.0.0.1:7421");

        assert!(parse_peers("").is_err());
        assert!(parse_peers("1:127.0.0.1:7420").is_err());
        assert!(parse_peers("abc=127.0.0.1:7420").is_err());
        assert!(parse_peers("1=").is_err());
        assert!(parse_peers("1=127.0.0.1:7420,1=127.0.0.1:7421").is_err());
    }
}
