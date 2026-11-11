//! The `peppydb` command line (plan 0026, "The binary").
//!
//! `bin/peppydb` calls [`main`]. The code lives here so that the process
//! tests can run exactly what the binary runs.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use crate::{Config, Exit, serve};

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
    let options = engine::Options {
        flush_interval: Duration::from_micros(args.flush_interval_us),
        io: match args.io {
            IoMode::Cached => engine::Io::Cached,
            IoMode::Direct => engine::Io::Direct,
        },
        ..engine::Options::default()
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Every permitted engine call gets a blocking thread at once.
        .max_blocking_threads(config.max_inflight.saturating_add(64))
        .build()?;
    let db = engine::Db::open(&args.data_dir, options)
        .map_err(|error| format!("cannot open {}: {error}", args.data_dir.display()))?;
    runtime.block_on(async move {
        let shutdown = shutdown_signal()?;
        let listener = tokio::net::TcpListener::bind(args.listen).await?;
        tracing::info!("listening on {}", listener.local_addr()?);
        Ok(serve(db, listener, config, shutdown).await?)
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
