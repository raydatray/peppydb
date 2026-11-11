//! Process tests: `peppydb serve` in a child process, killed with SIGKILL,
//! stopped with SIGTERM, and refused a locked database.
//!
//! The child is this test binary itself, running `child_server`, which
//! calls [`crate::cli::main`], the code `bin/peppydb` runs. Re-executing the
//! test binary works under Buck2 and Cargo without locating the binary.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use server_proto::kv::v1::kv_service_client::KvServiceClient;
use server_proto::kv::v1::{GetRequest, PutRequest};

/// Set in the child: the command-line arguments, separated by newlines.
const CHILD_ARGS: &str = "PEPPYDB_TEST_CHILD_ARGS";

/// The child process's entry point. Ignored, so it runs only when the
/// parent asks for it by name.
#[test]
#[ignore = "the child process of the process tests"]
fn child_server() {
    let Ok(args) = std::env::var(CHILD_ARGS) else {
        return;
    };
    let args = std::iter::once("peppydb").chain(args.split('\n'));
    let code = crate::cli::main(args);
    // Exit with the binary's code instead of the test harness's.
    std::process::exit(if code == std::process::ExitCode::SUCCESS {
        0
    } else {
        1
    });
}

/// A child `peppydb serve` process.
struct ServerProcess {
    child: Child,
    stderr: Arc<Mutex<String>>,
}

impl ServerProcess {
    /// Start `peppydb serve --data-dir <dir>` on a free loopback port.
    fn spawn(dir: &Path) -> (Self, mpsc::Receiver<SocketAddr>) {
        let args = format!(
            "serve\n--data-dir\n{}\n--listen\n127.0.0.1:0",
            dir.display()
        );
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::process::child_server",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD_ARGS, args)
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let (addr_tx, addr_rx) = mpsc::channel();
        let pipe = child.stderr.take().unwrap();
        let log = Arc::clone(&stderr);
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                let Ok(line) = line else { return };
                if let Some(addr) = line.split("listening on ").nth(1) {
                    let _ = addr_tx.send(addr.trim().parse().unwrap());
                }
                let mut log = log.lock().unwrap();
                log.push_str(&line);
                log.push('\n');
            }
        });
        (Self { child, stderr }, addr_rx)
    }

    /// Start a child and wait until it listens.
    fn start(dir: &Path) -> (Self, SocketAddr) {
        let (child, addr) = Self::spawn(dir);
        let addr = addr
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|_| panic!("no address; stderr:\n{}", child.stderr()));
        (child, addr)
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    fn sigkill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    fn sigterm(&self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn wait(&mut self) -> ExitStatus {
        for _ in 0..600 {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the child did not exit; stderr:\n{}", self.stderr());
    }
}

async fn client(addr: SocketAddr) -> KvServiceClient<tonic::transport::Channel> {
    KvServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap()
}

fn key(i: u32) -> Vec<u8> {
    i.to_be_bytes().to_vec()
}

#[tokio::test(flavor = "multi_thread")]
async fn acknowledged_writes_survive_sigkill_and_sigterm_stops_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, addr) = ServerProcess::start(dir.path());
    let mut kv = client(addr).await;
    // Concurrent writers, so the kill can land while syncs are in flight.
    let mut writers = Vec::new();
    for w in 0..4u32 {
        let mut kv = kv.clone();
        writers.push(tokio::spawn(async move {
            let mut acknowledged = Vec::new();
            for i in (w..).step_by(4) {
                let request = PutRequest {
                    key: key(i),
                    value: i.to_le_bytes().to_vec(),
                };
                match kv.put(request).await {
                    Ok(_) => acknowledged.push(i),
                    Err(_) => return acknowledged,
                }
            }
            unreachable!()
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    child.sigkill();
    let mut acknowledged = Vec::new();
    for writer in writers {
        acknowledged.extend(writer.await.unwrap());
    }
    assert!(!acknowledged.is_empty());
    drop(kv);

    let (mut child, addr) = ServerProcess::start(dir.path());
    kv = client(addr).await;
    for &i in &acknowledged {
        let value = kv
            .get(GetRequest { key: key(i) })
            .await
            .unwrap()
            .into_inner()
            .value;
        assert_eq!(value, Some(i.to_le_bytes().to_vec()), "lost key {i}");
    }

    child.sigterm();
    let status = child.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", child.stderr());
    assert!(child.stderr().contains("stopped cleanly"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_server_on_a_locked_database_exits_with_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (first, _addr) = ServerProcess::start(dir.path());

    let (mut second, _) = ServerProcess::spawn(dir.path());
    let status = second.wait();
    assert_eq!(status.code(), Some(1));
    assert!(
        second.stderr().contains("another process"),
        "stderr:\n{}",
        second.stderr()
    );

    let mut first = first;
    first.sigterm();
    assert_eq!(first.wait().code(), Some(0));
}
