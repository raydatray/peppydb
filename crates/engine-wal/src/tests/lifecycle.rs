//! Truncation, concurrency, simulated crashes, and flusher failures.

use std::time::Duration;

use super::writer::read_all;
use crate::{Op, Wal, WalError, WalOptions, segment};

fn put<'a>(key: &'a [u8], value: &'a [u8]) -> Op<'a> {
    Op::Put { key, value }
}

/// A log with one record per segment, sequences 1..=n.
fn rotated(dir: &std::path::Path, n: u32) -> Wal {
    let options = WalOptions {
        segment_bytes: 1,
        ..WalOptions::default()
    };
    let wal = Wal::open(dir, options, 1).unwrap();
    for i in 0..n {
        let seq = wal.append(put(&i.to_be_bytes(), b"v")).unwrap();
        wal.wait_durable(seq).unwrap();
    }
    wal
}

fn names(dir: &std::path::Path) -> Vec<u64> {
    segment::list(dir)
        .unwrap()
        .into_iter()
        .map(|(first, _)| first)
        .collect()
}

#[test]
fn truncation_deletes_exactly_the_redundant_sealed_segments() {
    let dir = tempfile::tempdir().unwrap();
    let wal = rotated(dir.path(), 6);
    let before = names(dir.path());
    assert_eq!(before, [1, 2, 3, 4, 5, 6], "one record per segment");
    assert_eq!(wal.truncate_below(0).unwrap(), 0);
    // Segments 1 and 2 hold only records at or below 2.
    assert_eq!(wal.truncate_below(2).unwrap(), 2);
    assert_eq!(names(dir.path()), [3, 4, 5, 6]);
    assert_eq!(wal.truncate_below(2).unwrap(), 0, "idempotent");
    // Never the last segment, even when every record is covered.
    assert_eq!(wal.truncate_below(100).unwrap(), 3);
    assert_eq!(names(dir.path()), [6]);
    wal.close().unwrap();
    let mut seen = Vec::new();
    Wal::replay(dir.path(), 6, |record| {
        seen.push(record.seq);
        Ok::<_, WalError>(())
    })
    .unwrap();
    assert_eq!(seen, [6]);
}

#[test]
fn concurrent_truncations_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let wal = rotated(dir.path(), 40);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| wal.truncate_below(30).unwrap());
        }
    });
    // Segments 1..=30 hold only records at or below 30. (The per-call
    // counts may overlap: two unlinks of one path can both succeed.)
    assert_eq!(names(dir.path()), (31..=40).collect::<Vec<_>>());
}

#[test]
fn eight_appenders_get_contiguous_sequences_and_share_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::open(dir.path(), WalOptions::default(), 1).unwrap();
    let mut seqs: Vec<u64> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8u8)
            .map(|t| {
                let wal = &wal;
                scope.spawn(move || {
                    (0..400u16)
                        .map(|i| {
                            let key = [t, (i >> 8) as u8, i as u8];
                            let seq = wal.append(put(&key, &[t; 8])).unwrap();
                            wal.wait_durable(seq).unwrap();
                            seq
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    let syncs = wal.syncs();
    wal.close().unwrap();
    seqs.sort_unstable();
    assert_eq!(
        seqs,
        (1..=3200).collect::<Vec<_>>(),
        "distinct and contiguous"
    );
    assert!(syncs < 3200, "{syncs} syncs for 3200 records");
    let replayed: Vec<_> = read_all(dir.path()).into_iter().map(|r| r.0).collect();
    assert_eq!(replayed, (1..=3200).collect::<Vec<_>>());
}

/// A long flush interval keeps appends pending until `sync_to`.
fn slow(dir: &std::path::Path) -> Wal {
    let options = WalOptions {
        flush_interval: Duration::from_secs(30),
        ..WalOptions::default()
    };
    Wal::open(dir, options, 1).unwrap()
}

#[test]
fn abandon_loses_only_records_never_reported_durable() {
    let dir = tempfile::tempdir().unwrap();
    let wal = slow(dir.path());
    for i in 0..5u8 {
        wal.append(put(&[i], b"durable")).unwrap();
    }
    wal.sync_to(5).unwrap();
    for i in 5..9u8 {
        wal.append(put(&[i], b"lost")).unwrap();
    }
    assert_eq!(wal.durable_seq(), 5);
    wal.abandon(0);
    let replayed: Vec<_> = read_all(dir.path()).into_iter().map(|r| r.0).collect();
    assert_eq!(replayed, [1, 2, 3, 4, 5]);
}

#[test]
fn abandon_at_every_byte_of_a_record_leaves_a_repairable_tail() {
    let mut one = Vec::new();
    crate::record::encode_op(&mut one, 6, put(&[5], b"lost"));
    for written in 0..=one.len() {
        let dir = tempfile::tempdir().unwrap();
        let wal = slow(dir.path());
        for i in 0..5u8 {
            wal.append(put(&[i], b"durable")).unwrap();
        }
        wal.sync_to(5).unwrap();
        wal.append(put(&[5], b"lost")).unwrap();
        wal.abandon(written);
        let replayed: Vec<_> = read_all(dir.path()).into_iter().map(|r| r.0).collect();
        let expected: Vec<u64> = if written == one.len() {
            (1..=6).collect()
        } else {
            (1..=5).collect()
        };
        assert_eq!(replayed, expected, "written {written}");
    }
}

#[test]
fn a_flusher_failure_is_sticky() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::open(dir.path(), WalOptions::default(), 1).unwrap();
    let seq = wal.append(put(b"a", b"1")).unwrap();
    wal.wait_durable(seq).unwrap();
    wal.fail_next_sync();
    let seq = wal.append(put(b"b", b"2")).unwrap();
    assert!(matches!(
        wal.wait_durable(seq),
        Err(WalError::Failed {
            op: "sync WAL group",
            ..
        })
    ));
    assert!(matches!(
        wal.append(put(b"c", b"3")),
        Err(WalError::Failed { .. })
    ));
    assert!(matches!(wal.sync_to(seq), Err(WalError::Failed { .. })));
    assert!(matches!(
        wal.close(),
        Err(WalError::Io {
            op: "sync WAL group",
            ..
        })
    ));
    assert!(wal.close().is_ok(), "the original error is returned once");
}

#[test]
fn appended_bytes_count_every_record() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::open(dir.path(), WalOptions::default(), 1).unwrap();
    assert_eq!(wal.appended_bytes(), 0);
    wal.append(put(b"a", b"1")).unwrap();
    let one = wal.appended_bytes();
    assert!(one > 0);
    wal.append_batch(&[put(b"a", b"1"), put(b"b", b"2")])
        .unwrap();
    assert!(wal.appended_bytes() > 2 * one);
}
