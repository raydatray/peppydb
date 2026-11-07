//! Appends, group commit, rotation, and reopening.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::{MAX_RECORD_BYTES, Op, Wal, WalError, WalOptions};

fn put<'a>(key: &'a [u8], value: &'a [u8]) -> Op<'a> {
    Op::Put { key, value }
}

/// An owned record: its sequence, and each operation's key and value
/// (`None` for a delete).
pub(super) type Owned = (u64, Vec<(Vec<u8>, Option<Vec<u8>>)>);

/// Every record in the log from 1.
pub(super) fn read_all(dir: &Path) -> Vec<Owned> {
    let mut out = Vec::new();
    Wal::replay(dir, 1, |record| {
        let ops = record
            .ops
            .iter()
            .map(|op| match *op {
                Op::Put { key, value } => (key.to_vec(), Some(value.to_vec())),
                Op::Delete { key } => (key.to_vec(), None),
            })
            .collect();
        out.push((record.seq, ops));
        Ok::<_, WalError>(())
    })
    .unwrap();
    out
}

#[test]
fn appends_round_trip_through_close_and_replay() {
    for interval in [Duration::ZERO, Duration::from_millis(10)] {
        let dir = tempfile::tempdir().unwrap();
        let options = WalOptions {
            flush_interval: interval,
            ..WalOptions::default()
        };
        let wal = Wal::open(dir.path(), options, 1).unwrap();
        assert_eq!(wal.append(put(b"a", b"1")).unwrap(), 1);
        let seq = wal
            .append_batch(&[put(b"b", b"2"), Op::Delete { key: b"a" }])
            .unwrap();
        assert_eq!(seq, 2);
        let seq = wal.append(Op::Delete { key: b"b" }).unwrap();
        wal.wait_durable(seq).unwrap();
        assert_eq!(wal.durable_seq(), 3);
        wal.close().unwrap();
        let records = read_all(dir.path());
        assert_eq!(records.len(), 3);
        assert_eq!(records[1].1.len(), 2);
        assert_eq!(records[2].1, vec![(b"b".to_vec(), None)]);
    }
}

#[test]
fn sync_to_skips_a_long_flush_interval() {
    let dir = tempfile::tempdir().unwrap();
    let options = WalOptions {
        flush_interval: Duration::from_secs(1),
        ..WalOptions::default()
    };
    let wal = Wal::open(dir.path(), options, 1).unwrap();
    let seq = wal.append(put(b"a", b"1")).unwrap();
    let started = Instant::now();
    wal.sync_to(seq).unwrap();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(wal.durable_seq(), seq);
}

#[test]
fn segments_rotate_and_are_named_by_their_first_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let options = WalOptions {
        segment_bytes: 200,
        ..WalOptions::default()
    };
    let wal = Wal::open(dir.path(), options, 1).unwrap();
    for i in 0..50u32 {
        let seq = wal.append(put(&i.to_be_bytes(), &[7; 20])).unwrap();
        wal.wait_durable(seq).unwrap(); // one group per record
    }
    wal.close().unwrap();
    let segments = crate::segment::list(dir.path()).unwrap();
    assert!(segments.len() > 5, "{} segments", segments.len());
    assert_eq!(segments[0].0, 1);
    let records = read_all(dir.path());
    assert_eq!(
        records.iter().map(|r| r.0).collect::<Vec<_>>(),
        (1..=50).collect::<Vec<_>>()
    );
    // Each segment's name is the sequence of its first record.
    for (first_seq, path) in segments {
        let bytes = std::fs::read(path).unwrap();
        if bytes.len() > crate::record::HEADER_BYTES {
            let (record, _) = crate::record::decode(&bytes[crate::record::HEADER_BYTES..]).unwrap();
            assert_eq!(record.seq, first_seq);
        }
    }
}

#[test]
fn appends_after_close_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::open(dir.path(), WalOptions::default(), 1).unwrap();
    wal.close().unwrap();
    assert!(matches!(wal.append(put(b"a", b"1")), Err(WalError::Closed)));
    wal.close().unwrap();
}

#[test]
fn an_oversized_batch_takes_no_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::open(dir.path(), WalOptions::default(), 1).unwrap();
    let big = vec![0u8; MAX_RECORD_BYTES / 2];
    let ops = [put(b"a", &big), put(b"b", &big)];
    assert!(matches!(
        wal.append_batch(&ops),
        Err(WalError::RecordTooLarge { .. })
    ));
    assert_eq!(wal.append(put(b"c", b"1")).unwrap(), 1);
    // The largest record that fits is accepted.
    let fits = vec![0u8; MAX_RECORD_BYTES - 9 - 8 - 1];
    assert_eq!(wal.append(put(b"d", &fits)).unwrap(), 2);
    wal.close().unwrap();
    assert_eq!(read_all(dir.path()).len(), 2);
}

#[test]
fn reopening_continues_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::open(dir.path(), WalOptions::default(), 1).unwrap();
    for i in 0..5u8 {
        wal.append(put(&[i], b"v")).unwrap();
    }
    wal.close().unwrap();
    let last = Wal::replay(dir.path(), 1, |_| Ok::<_, WalError>(()))
        .unwrap()
        .last_seq;
    assert_eq!(last, Some(5));
    let wal = Wal::open(dir.path(), WalOptions::default(), 6).unwrap();
    assert_eq!(wal.append(put(b"x", b"v")).unwrap(), 6);
    wal.close().unwrap();
    // Reopening with nothing appended leaves a header-only segment, which
    // the next open replaces.
    drop(Wal::open(dir.path(), WalOptions::default(), 7).unwrap());
    let wal = Wal::open(dir.path(), WalOptions::default(), 7).unwrap();
    assert_eq!(wal.append(put(b"y", b"v")).unwrap(), 7);
    wal.close().unwrap();
    assert_eq!(
        read_all(dir.path()).iter().map(|r| r.0).collect::<Vec<_>>(),
        (1..=7).collect::<Vec<_>>()
    );
}

#[test]
fn sequence_zero_means_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::open(dir.path(), WalOptions::default(), 0).unwrap();
    assert_eq!(wal.durable_seq(), 0);
    let seq = wal.append(put(b"a", b"1")).unwrap();
    assert_eq!(seq, 1);
    wal.wait_durable(seq).unwrap();
    wal.close().unwrap();
    let replayed = Wal::replay(dir.path(), 0, |_| Ok::<_, WalError>(())).unwrap();
    assert_eq!(replayed.records, 1);
}
