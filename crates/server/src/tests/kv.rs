//! `Get`, `Put`, and `Delete` over gRPC, the key layout, and the format
//! marker.

use std::collections::BTreeMap;

use server_proto::kv::v1::{DeleteRequest, GetRequest, PutRequest};
use tonic::Code;

use super::{Rng, Server};
use crate::keys::{FORMAT_KEY, FORMAT_VERSION};
use crate::{Config, Exit, FormatError, ServeError, serve};

fn put(key: &[u8], value: &[u8]) -> PutRequest {
    PutRequest {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn get(key: &[u8]) -> GetRequest {
    GetRequest { key: key.to_vec() }
}

fn delete(key: &[u8]) -> DeleteRequest {
    DeleteRequest { key: key.to_vec() }
}

fn code<T>(result: Result<T, tonic::Status>) -> Code {
    result.map(|_| ()).unwrap_err().code()
}

#[tokio::test(flavor = "multi_thread")]
async fn point_operations_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;
    let mut kv = server.kv().await;

    assert_eq!(kv.get(get(b"a")).await.unwrap().into_inner().value, None);
    kv.put(put(b"a", b"1")).await.unwrap();
    assert_eq!(
        kv.get(get(b"a")).await.unwrap().into_inner().value,
        Some(b"1".to_vec())
    );
    kv.put(put(b"a", b"2")).await.unwrap();
    assert_eq!(
        kv.get(get(b"a")).await.unwrap().into_inner().value,
        Some(b"2".to_vec())
    );

    // An empty value is a value, not an absence.
    kv.put(put(b"empty", b"")).await.unwrap();
    assert_eq!(
        kv.get(get(b"empty")).await.unwrap().into_inner().value,
        Some(Vec::new())
    );

    kv.delete(delete(b"a")).await.unwrap();
    assert_eq!(kv.get(get(b"a")).await.unwrap().into_inner().value, None);
    // Deleting an absent key succeeds.
    kv.delete(delete(b"never")).await.unwrap();

    assert!(matches!(server.stop().await, Ok(Exit::Clean)));
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_requests_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;
    let mut kv = server.kv().await;
    let limits = engine::TreeConfig::default();
    // The stored key carries one namespace byte, so the largest user key is
    // one byte shorter than the engine's limit.
    let longest = vec![b'k'; limits.max_key_len - 1];
    let too_long = vec![b'k'; limits.max_key_len];
    let too_large = vec![0; limits.max_value_len + 1];

    assert_eq!(code(kv.put(put(b"", b"v")).await), Code::InvalidArgument);
    assert_eq!(code(kv.get(get(b"")).await), Code::InvalidArgument);
    assert_eq!(code(kv.delete(delete(b"")).await), Code::InvalidArgument);
    assert_eq!(
        code(kv.put(put(&too_long, b"v")).await),
        Code::InvalidArgument
    );
    assert_eq!(
        code(kv.put(put(b"k", &too_large)).await),
        Code::InvalidArgument
    );
    kv.put(put(&longest, b"v")).await.unwrap();
    server.stop().await.unwrap();

    // Only the format marker and the one valid write reached the engine.
    let db = engine::Db::open(dir.path(), engine::Options::default()).unwrap();
    let stored: Vec<_> = db
        .range(None, None)
        .unwrap()
        .map(|record| record.unwrap().0)
        .collect();
    assert_eq!(
        stored,
        [FORMAT_KEY.to_vec(), [&[0x01], &longest[..]].concat()]
    );
    db.close().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn user_keys_are_stored_under_the_user_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;
    let mut kv = server.kv().await;
    // Keys that look like system keys or reserved prefixes are ordinary
    // user keys.
    for key in [&b"\x00format"[..], b"\x00", b"\xff\xff", b"plain"] {
        kv.put(put(key, key)).await.unwrap();
        assert_eq!(
            kv.get(get(key))
                .await
                .unwrap()
                .into_inner()
                .value
                .as_deref(),
            Some(key)
        );
    }
    server.stop().await.unwrap();

    let db = engine::Db::open(dir.path(), engine::Options::default()).unwrap();
    assert_eq!(db.get(FORMAT_KEY).unwrap().as_deref(), Some(FORMAT_VERSION));
    for key in [&b"\x00format"[..], b"\x00", b"\xff\xff", b"plain"] {
        let stored = [&[0x01], key].concat();
        assert_eq!(db.get(&stored).unwrap().as_deref(), Some(key));
    }
    db.close().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_database_without_the_format_marker_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = engine::Db::open(dir.path(), engine::Options::default()).unwrap();
    db.put(b"written directly", b"v").unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let result = serve(db, listener, Config::default(), std::future::pending()).await;
    assert!(matches!(
        result,
        Err(ServeError::Format(FormatError::Missing))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;
    let mut kv = server.kv().await;
    for i in 0..100u32 {
        kv.put(put(&i.to_be_bytes(), &i.to_le_bytes()))
            .await
            .unwrap();
    }
    kv.delete(delete(&7u32.to_be_bytes())).await.unwrap();
    server.stop().await.unwrap();

    let server = Server::start(dir.path(), Config::default()).await;
    let mut kv = server.kv().await;
    for i in 0..100u32 {
        let value = kv
            .get(get(&i.to_be_bytes()))
            .await
            .unwrap()
            .into_inner()
            .value;
        let expected = (i != 7).then(|| i.to_le_bytes().to_vec());
        assert_eq!(value, expected, "key {i}");
    }
    server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_clients_match_the_oracle() {
    const CLIENTS: u64 = 8;
    const OPS: u64 = 200;
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;

    // Each client owns the keys that start with its id, so its own history
    // is the oracle for them.
    let mut tasks = Vec::new();
    for client in 0..CLIENTS {
        let mut kv = server.kv().await;
        tasks.push(tokio::spawn(async move {
            let mut rng = Rng::new(client + 1);
            let mut oracle = BTreeMap::new();
            for op in 0..OPS {
                let key = vec![client as u8, rng.below(16) as u8];
                match rng.below(10) {
                    0..=5 => {
                        let value = op.to_le_bytes().to_vec();
                        kv.put(put(&key, &value)).await.unwrap();
                        oracle.insert(key, value);
                    }
                    6..=7 => {
                        kv.delete(delete(&key)).await.unwrap();
                        oracle.remove(&key);
                    }
                    _ => {
                        let value = kv.get(get(&key)).await.unwrap().into_inner().value;
                        assert_eq!(value.as_ref(), oracle.get(&key));
                    }
                }
            }
            oracle
        }));
    }
    let mut oracle = BTreeMap::new();
    for task in tasks {
        oracle.extend(task.await.unwrap());
    }
    server.stop().await.unwrap();

    let server = Server::start(dir.path(), Config::default()).await;
    let mut kv = server.kv().await;
    for client in 0..CLIENTS {
        for k in 0..16u8 {
            let key = vec![client as u8, k];
            let value = kv.get(get(&key)).await.unwrap().into_inner().value;
            assert_eq!(value.as_ref(), oracle.get(&key), "key {key:?}");
        }
    }
    server.stop().await.unwrap();
}
