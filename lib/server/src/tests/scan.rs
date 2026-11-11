//! Paginated `Scan`.

use std::collections::BTreeMap;

use kv_proto::kv::v1::kv_service_client::KvServiceClient;
use kv_proto::kv::v1::{PutRequest, ScanRequest, ScanResponse};
use tonic::Code;
use tonic::transport::Channel;

use super::{Rng, Server};
use crate::Config;

async fn fill(kv: &mut KvServiceClient<Channel>, records: &BTreeMap<Vec<u8>, Vec<u8>>) {
    for (key, value) in records {
        kv.put(PutRequest {
            key: key.clone(),
            value: value.clone(),
        })
        .await
        .unwrap();
    }
}

async fn page(
    kv: &mut KvServiceClient<Channel>,
    start: Option<&[u8]>,
    end: Option<&[u8]>,
    page_size: u32,
) -> ScanResponse {
    kv.scan(ScanRequest {
        start: start.map(<[u8]>::to_vec),
        end: end.map(<[u8]>::to_vec),
        page_size,
    })
    .await
    .unwrap()
    .into_inner()
}

/// Every record of `[start, end)`, page by page, with the page lengths.
async fn scan_all(
    kv: &mut KvServiceClient<Channel>,
    start: Option<&[u8]>,
    end: Option<&[u8]>,
    page_size: u32,
) -> (Vec<(Vec<u8>, Vec<u8>)>, Vec<usize>) {
    let mut records = Vec::new();
    let mut pages = Vec::new();
    let mut next = start.map(<[u8]>::to_vec);
    loop {
        let response = page(kv, next.as_deref(), end, page_size).await;
        pages.push(response.records.len());
        records.extend(response.records.into_iter().map(|r| (r.key, r.value)));
        match response.next_start {
            Some(start) => next = Some(start),
            None => return (records, pages),
        }
    }
}

fn expected(
    oracle: &BTreeMap<Vec<u8>, Vec<u8>>,
    start: Option<&[u8]>,
    end: Option<&[u8]>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    oracle
        .iter()
        .filter(|(key, _)| start.is_none_or(|start| key.as_slice() >= start))
        .filter(|(key, _)| end.is_none_or(|end| key.as_slice() < end))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn paging_returns_each_record_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        scan_max_records: 50,
        ..Config::default()
    };
    let server = Server::start(dir.path(), config).await;
    let mut kv = server.kv().await;
    let mut rng = Rng::new(7);
    let mut oracle = BTreeMap::new();
    for _ in 0..400 {
        let len = 1 + rng.below(3) as usize;
        let key: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
        oracle.insert(key, rng.next().to_le_bytes().to_vec());
    }
    fill(&mut kv, &oracle).await;

    for round in 0..40 {
        let bound = |rng: &mut Rng| {
            (rng.below(4) != 0).then(|| vec![rng.below(256) as u8, rng.below(256) as u8])
        };
        let (mut start, mut end) = (bound(&mut rng), bound(&mut rng));
        if let (Some(s), Some(e)) = (&start, &end)
            && s > e
        {
            std::mem::swap(&mut start, &mut end);
        }
        let page_size = [0, 1, 7, 50, 1000][round % 5];
        let (records, pages) = scan_all(&mut kv, start.as_deref(), end.as_deref(), page_size).await;
        assert_eq!(
            records,
            expected(&oracle, start.as_deref(), end.as_deref()),
            "{start:?}..{end:?} page size {page_size}"
        );
        let max = match page_size {
            0 => 50,
            n => n.min(50) as usize,
        };
        assert!(pages.iter().all(|&len| len <= max), "{pages:?} over {max}");
    }
    server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn pages_respect_the_record_and_byte_limits() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        scan_max_records: 10,
        scan_max_bytes: 1000,
        ..Config::default()
    };
    let server = Server::start(dir.path(), config).await;
    let mut kv = server.kv().await;
    // 30 records of 2 + 300 bytes.
    let oracle: BTreeMap<_, _> = (0..30u16)
        .map(|i| (i.to_be_bytes().to_vec(), vec![b'v'; 300]))
        .collect();
    fill(&mut kv, &oracle).await;

    // The byte limit binds first: a page stops once it reaches 1000 bytes,
    // so it holds 4 records (the fourth crosses the limit).
    let first = page(&mut kv, None, None, 0).await;
    assert_eq!(first.records.len(), 4);
    assert_eq!(first.next_start.as_deref(), Some(&4u16.to_be_bytes()[..]));

    // A page size above the maximum uses the maximum; a small one binds.
    let small: BTreeMap<_, _> = (0..30u16)
        .map(|i| ([b"s", &i.to_be_bytes()[..]].concat(), b"v".to_vec()))
        .collect();
    fill(&mut kv, &small).await;
    let (start, end) = (Some(&b"s"[..]), Some(&b"t"[..]));
    assert_eq!(page(&mut kv, start, end, 0).await.records.len(), 10);
    assert_eq!(page(&mut kv, start, end, 1000).await.records.len(), 10);
    assert_eq!(page(&mut kv, start, end, 3).await.records.len(), 3);

    // next_start is unset exactly when the range is exhausted.
    let last = page(&mut kv, Some(&[b's', 0, 25]), end, 5).await;
    assert_eq!(last.records.len(), 5);
    assert_eq!(last.next_start, None);
    let empty = page(&mut kv, Some(b"x"), Some(b"y"), 0).await;
    assert!(empty.records.is_empty());
    assert_eq!(empty.next_start, None);
    server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unbounded_scan_returns_user_keys_only() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;
    let mut kv = server.kv().await;
    let oracle: BTreeMap<_, _> = [&b"\x00format"[..], b"\x00", b"a", b"\xff", b"\xff\xff\xff"]
        .into_iter()
        .map(|key| (key.to_vec(), b"v".to_vec()))
        .collect();
    fill(&mut kv, &oracle).await;

    // The stored format marker (0x00 ‖ "format") is not a user record.
    let (records, _) = scan_all(&mut kv, None, None, 0).await;
    assert_eq!(records, expected(&oracle, None, None));
    server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reversed_range_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), Config::default()).await;
    let mut kv = server.kv().await;
    let status = kv
        .scan(ScanRequest {
            start: Some(b"b".to_vec()),
            end: Some(b"a".to_vec()),
            page_size: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    server.stop().await.unwrap();
}
