//! `Status`, `Checkpoint`, health, graceful shutdown, and fail-stop.

use std::time::{Duration, Instant};

use kv_proto::kv::v1::{CheckpointRequest, GetRequest, PutRequest, StatusRequest};
use tonic::Code;
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
    assert_eq!(second.checkpoint_id, first.checkpoint_id + 1);
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
    let db = super::open_db(dir.path());
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
async fn a_wal_failure_stops_the_server_and_a_reopen_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;
    let mut kv = server.kv().await;
    kv.put(put(b"acknowledged")).await.unwrap();

    server.fail_next_wal_sync().await;
    let status = kv.put(put(b"failed")).await.unwrap_err();
    assert_eq!(status.code(), Code::Unavailable);
    // The server stops on its own, without a shutdown signal.
    let exit = tokio::time::timeout(Duration::from_secs(30), server.exited())
        .await
        .expect("the server stops after a WAL failure");
    assert!(matches!(exit, Ok(Exit::Fatal(engine::Error::Wal(_)))));

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
