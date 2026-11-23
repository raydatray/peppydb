//! `Status`, `Checkpoint`, `Snapshot`, health, graceful shutdown, and fail-stop.

use std::io::Cursor;
use std::time::{Duration, Instant};

use kv_proto::kv::v1::snapshot_response::Payload;
use kv_proto::kv::v1::{
    CheckpointRequest, GetRequest, PutRequest, SnapshotRequest, SnapshotTrailer, StatusRequest,
};
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;

use super::Server;
use crate::{Config, Exit};

fn put(key: &[u8]) -> PutRequest {
    PutRequest {
        key: key.to_vec(),
        value: b"v".to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_checkpoints_report_engine_progress() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;
    let (mut kv, mut admin) = (server.kv().await, server.admin().await);

    let before = admin.status(StatusRequest {}).await.unwrap().into_inner();
    assert!(!before.version.is_empty());
    assert_eq!(before.node_id, 1);
    assert_eq!(before.role, "leader");
    assert!(before.term >= 1);
    assert_eq!(before.leader_id, Some(1));
    assert_eq!(before.leader_addr, Some(server.addr.to_string()));
    for key in [b"a", b"b", b"c"] {
        kv.put(put(key)).await.unwrap();
    }
    let after = admin.status(StatusRequest {}).await.unwrap().into_inner();
    assert_eq!(after.applied_seq, before.applied_seq + 3);

    let first = admin
        .checkpoint(CheckpointRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.applied_seq, after.applied_seq);
    kv.put(put(b"d")).await.unwrap();
    let second = admin
        .checkpoint(CheckpointRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(second.checkpoint_id > first.checkpoint_id);
    assert_eq!(second.applied_seq, first.applied_seq + 1);
    server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_with_writes_in_flight_is_clean_and_keeps_every_acknowledged_write() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;
    let mut writers = Vec::new();
    for writer in 0..8u8 {
        let mut kv = server.kv().await;
        writers.push(tokio::spawn(async move {
            let mut acknowledged = Vec::new();
            for i in 0u32.. {
                let key = [&[writer][..], &i.to_be_bytes()].concat();
                match kv.put(put(&key)).await {
                    Ok(_) => acknowledged.push(key),
                    Err(_) => return acknowledged,
                }
            }
            unreachable!()
        }));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(matches!(server.stop().await, Ok(Exit::Clean)));

    let mut acknowledged = Vec::new();
    for writer in writers {
        acknowledged.extend(writer.await.unwrap());
    }
    assert!(!acknowledged.is_empty());
    // The close released the lock, and every acknowledged write is durable.
    let db = super::open_external_db(&dir.path().join("db"));
    for key in &acknowledged {
        let stored = [&[0x01], &key[..]].concat();
        assert!(db.get(&stored).unwrap().is_some(), "lost {key:?}");
    }
    db.close().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn health_turns_not_serving_and_watchers_do_not_delay_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        shutdown_grace: Duration::from_secs(30),
        ..Config::default()
    };
    let server = Server::start(dir.path(), config).await;
    let mut health = server.health().await;
    for service in ["", "peppydb.kv.v1.KvService", "peppydb.kv.v1.AdminService"] {
        let request = HealthCheckRequest {
            service: service.to_owned(),
        };
        let status = health.check(request).await.unwrap().into_inner().status;
        assert_eq!(status, ServingStatus::Serving as i32, "{service:?}");
    }
    let mut watch = health
        .watch(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let first = watch.message().await.unwrap().unwrap();
    assert_eq!(first.status, ServingStatus::Serving as i32);

    let started = Instant::now();
    let stopping = tokio::spawn(server.stop());
    let second = watch.message().await.unwrap().unwrap();
    assert_eq!(second.status, ServingStatus::NotServing as i32);
    assert!(matches!(stopping.await.unwrap(), Ok(Exit::Clean)));
    // Far below the 30 s grace period: the open watch did not hold it up.
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fatal_failure_stops_the_server_and_a_reopen_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;
    let mut kv = server.kv().await;
    kv.put(put(b"acknowledged")).await.unwrap();

    server.trigger_fatal("injected raft storage failure");
    // The server stops on its own, without a shutdown signal.
    let exit = tokio::time::timeout(Duration::from_secs(30), server.exited())
        .await
        .expect("the server stops after a fatal failure");
    assert!(matches!(exit, Ok(Exit::Fatal(_))));

    // A restart reopens the database: the acknowledged write is there, and
    // writes work again.
    let server = Server::start(dir.path(), Config::default()).await;
    let mut kv = server.kv().await;
    let value = kv
        .get(GetRequest {
            key: b"acknowledged".to_vec(),
        })
        .await
        .unwrap()
        .into_inner()
        .value;
    assert_eq!(value.as_deref(), Some(&b"v"[..]));
    kv.put(put(b"after")).await.unwrap();
    server.stop().await.unwrap();
}

async fn receive_snapshot(
    admin: &mut kv_proto::kv::v1::admin_service_client::AdminServiceClient<
        tonic::transport::Channel,
    >,
    checkpoint: bool,
    max_chunk_bytes: usize,
) -> (Vec<u8>, SnapshotTrailer, usize) {
    let mut stream = admin
        .snapshot(SnapshotRequest { checkpoint })
        .await
        .unwrap()
        .into_inner();
    let mut bytes = Vec::new();
    let mut chunks = 0usize;
    let mut trailer = None;
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.payload.expect("snapshot payload") {
            Payload::Chunk(chunk) => {
                assert!(trailer.is_none(), "chunk after trailer");
                assert!(!chunk.is_empty(), "empty chunk");
                assert!(
                    chunk.len() <= max_chunk_bytes,
                    "chunk {} exceeds limit {}",
                    chunk.len(),
                    max_chunk_bytes
                );
                chunks += 1;
                bytes.extend_from_slice(&chunk);
            }
            Payload::Trailer(t) => {
                assert!(trailer.is_none(), "duplicate trailer");
                trailer = Some(t);
            }
        }
    }
    let trailer = trailer.expect("snapshot trailer");
    assert_eq!(trailer.total_bytes, bytes.len() as u64);
    assert_eq!(trailer.crc32c, crc32c::crc32c(&bytes));
    (bytes, trailer, chunks)
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_streams_valid_image_and_restores_exact_state() {
    let dir = tempfile::tempdir().unwrap();
    let chunk_size = 4096;
    let config = Config {
        snapshot_chunk_bytes: chunk_size,
        ..Config::default()
    };
    let server = Server::start(dir.path(), config).await;
    let (mut kv, mut admin) = (server.kv().await, server.admin().await);

    for i in 0..100u32 {
        kv.put(PutRequest {
            key: format!("key-{i:04}").into_bytes(),
            value: vec![b'x'; 256],
        })
        .await
        .unwrap();
    }
    let status = admin.status(StatusRequest {}).await.unwrap().into_inner();

    // Without `checkpoint: true`, the snapshot reflects the last committed
    // checkpoint (before the 100 writes were checkpointed).
    let (_, before_trailer, _) = receive_snapshot(&mut admin, false, chunk_size).await;
    assert!(before_trailer.applied_seq < status.applied_seq);

    // With `checkpoint: true`, a fresh checkpoint commits first and the
    // streamed image contains all 100 writes across multiple chunks.
    let (bytes, trailer, chunks) = receive_snapshot(&mut admin, true, chunk_size).await;
    assert_eq!(trailer.applied_seq, status.applied_seq);
    assert!(trailer.checkpoint_id > before_trailer.checkpoint_id);
    assert!(chunks > 1, "expected multiple chunks, got {chunks}");
    server.stop().await.unwrap();

    // Restore the streamed image into a fresh database and verify every key.
    let restored_dir = tempfile::tempdir().unwrap();
    let restored_path = restored_dir.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let db = engine::Db::open(
            &restored_path,
            engine::Options {
                mode: engine::Mode::External,
                ..engine::Options::default()
            },
        )
        .unwrap();
        let mut cursor = Cursor::new(bytes);
        let db = db.install_snapshot(&mut cursor).unwrap();
        assert_eq!(db.applied_seq(), trailer.applied_seq);
        for i in 0..100u32 {
            let key = [&[0x01][..], format!("key-{i:04}").as_bytes()].concat();
            let value = db.get(&key).unwrap();
            assert_eq!(value.as_deref(), Some(&vec![b'x'; 256][..]));
        }
        db.close().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_cancellation_releases_permits_and_allows_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        snapshot_chunk_bytes: 512,
        max_concurrent_snapshots: 1,
        ..Config::default()
    };
    let server = Server::start(dir.path(), config).await;
    let (mut kv, mut admin) = (server.kv().await, server.admin().await);

    for i in 0..50u32 {
        kv.put(PutRequest {
            key: format!("k-{i:03}").into_bytes(),
            value: vec![b'z'; 256],
        })
        .await
        .unwrap();
    }
    admin.checkpoint(CheckpointRequest {}).await.unwrap();

    // Start a snapshot, read one chunk, and drop the stream early.
    let mut stream = admin
        .snapshot(SnapshotRequest { checkpoint: false })
        .await
        .unwrap()
        .into_inner();
    let first = stream.message().await.unwrap().unwrap();
    assert!(matches!(first.payload, Some(Payload::Chunk(_))));
    drop(stream);

    // With `max_concurrent_snapshots = 1`, a second snapshot only succeeds
    // once the cancelled one releases its permit.
    let (_, trailer, chunks) = tokio::time::timeout(
        Duration::from_secs(10),
        receive_snapshot(&mut admin, false, 512),
    )
    .await
    .expect("second snapshot after cancel");
    assert!(chunks > 1);
    assert!(trailer.total_bytes > 0);

    assert!(matches!(server.stop().await, Ok(Exit::Clean)));
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_streams_cleanly_while_writes_run() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;
    let mut writer_kv = server.kv().await;
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let writer = tokio::spawn(async move {
        let mut i = 0u32;
        while stop_rx.try_recv().is_err() {
            let _ = writer_kv
                .put(PutRequest {
                    key: format!("live-{i:06}").into_bytes(),
                    value: vec![b'w'; 64],
                })
                .await;
            i += 1;
        }
    });

    let mut admin = server.admin().await;
    for _ in 0..3 {
        let (bytes, trailer, _) =
            receive_snapshot(&mut admin, true, Config::default().snapshot_chunk_bytes).await;
        assert!(trailer.total_bytes > 0);
        let check_dir = tempfile::tempdir().unwrap();
        let check_path = check_dir.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            let db = engine::Db::open(
                &check_path,
                engine::Options {
                    mode: engine::Mode::External,
                    ..engine::Options::default()
                },
            )
            .unwrap();
            let mut cursor = Cursor::new(bytes);
            let db = db.install_snapshot(&mut cursor).unwrap();
            assert_eq!(db.applied_seq(), trailer.applied_seq);
            db.close().unwrap();
        })
        .await
        .unwrap();
    }

    let _ = stop_tx.send(());
    writer.await.unwrap();
    assert!(matches!(server.stop().await, Ok(Exit::Clean)));
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let mut pending = req.as_bytes();
    while !pending.is_empty() {
        stream.writable().await.unwrap();
        match stream.try_write(pending) {
            Ok(n) => pending = &pending[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => panic!("write failed: {e}"),
        }
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        stream.readable().await.unwrap();
        match stream.try_read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => panic!("read failed: {e}"),
        }
    }
    String::from_utf8(out).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_endpoint_reports_rpcs_snapshots_and_applied_seq() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start_with_metrics_listener(dir.path(), Config::default()).await;
    let metrics_addr = server.metrics_addr.unwrap();
    let (mut kv, mut admin) = (server.kv().await, server.admin().await);

    // Unknown path returns 404.
    let not_found = http_get(metrics_addr, "/other").await;
    assert!(not_found.starts_with("HTTP/1.1 404 Not Found\r\n"));

    // Perform a few RPCs, including an invalid request and a snapshot.
    kv.put(put(b"m1")).await.unwrap();
    kv.put(put(b"m2")).await.unwrap();
    kv.get(GetRequest {
        key: b"m1".to_vec(),
    })
    .await
    .unwrap();
    let _ = kv.get(GetRequest { key: Vec::new() }).await.unwrap_err();
    let (_, trailer, _) =
        receive_snapshot(&mut admin, true, Config::default().snapshot_chunk_bytes).await;
    let status = admin.status(StatusRequest {}).await.unwrap().into_inner();

    let resp = http_get(metrics_addr, "/metrics").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
    let (_, body) = resp.split_once("\r\n\r\n").unwrap();
    assert!(body.contains("peppydb_rpc_requests_total{method=\"put\",code=\"ok\"} 2"));
    assert!(body.contains("peppydb_rpc_requests_total{method=\"get\",code=\"ok\"} 1"));
    assert!(
        body.contains("peppydb_rpc_requests_total{method=\"get\",code=\"invalid_argument\"} 1")
    );
    assert!(body.contains("peppydb_rpc_requests_total{method=\"snapshot\",code=\"ok\"} 1"));
    assert!(body.contains("peppydb_rpc_requests_total{method=\"status\",code=\"ok\"} 1"));
    assert!(body.contains("peppydb_rpc_duration_seconds_count{method=\"put\"} 2"));
    assert!(body.contains(&format!(
        "peppydb_snapshot_bytes_total {}",
        trailer.total_bytes
    )));
    assert!(body.contains(&format!(
        "peppydb_engine_applied_seq {}",
        status.applied_seq
    )));
    assert!(body.contains(&format!("peppydb_raft_term {}", status.term)));
    assert!(body.contains("peppydb_raft_role{role=\"leader\"} 1"));
    assert!(body.contains("peppydb_raft_role{role=\"follower\"} 0"));
    assert!(body.contains("peppydb_raft_commit_index "));
    assert!(body.contains("peppydb_raft_applied_index "));
    assert!(body.contains("peppydb_inflight_requests 0"));
    assert!(body.contains("peppydb_snapshot_active 0"));

    assert!(matches!(server.stop().await, Ok(Exit::Clean)));
}
