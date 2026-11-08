//! Replay over hand-built segment files.

use std::path::Path;

use crate::record::{HEADER_BYTES, encode_header, encode_op};
use crate::segment;
use crate::{Op, Replayed, Wal, WalError};

/// Write a segment holding one put per sequence in `seqs`, named by
/// `first`. Returns the file's bytes.
fn write_segment(dir: &Path, first: u64, seqs: impl IntoIterator<Item = u64>) -> Vec<u8> {
    let mut bytes = encode_header(first).to_vec();
    for seq in seqs {
        let key = seq.to_be_bytes();
        encode_op(
            &mut bytes,
            seq,
            Op::Put {
                key: &key,
                value: b"v",
            },
        );
    }
    std::fs::write(segment::path(dir, first), &bytes).unwrap();
    bytes
}

/// Replay from `from`, collecting the sequences applied.
fn replay(dir: &Path, from: u64) -> Result<(Vec<u64>, Replayed), WalError> {
    let mut seen = Vec::new();
    let replayed = Wal::replay(dir, from, |record| {
        assert_eq!(record.ops.len(), 1);
        seen.push(record.seq);
        Ok::<_, WalError>(())
    })?;
    Ok((seen, replayed))
}

fn record_bytes() -> usize {
    let mut buf = Vec::new();
    encode_op(
        &mut buf,
        1,
        Op::Put {
            key: &1u64.to_be_bytes(),
            value: b"v",
        },
    );
    buf.len()
}

#[test]
fn replays_from_the_start_and_from_the_middle() {
    let dir = tempfile::tempdir().unwrap();
    write_segment(dir.path(), 1, 1..=5);
    write_segment(dir.path(), 6, 6..=9);
    let (seen, replayed) = replay(dir.path(), 1).unwrap();
    assert_eq!(seen, (1..=9).collect::<Vec<_>>());
    assert_eq!(
        replayed,
        Replayed {
            last_seq: Some(9),
            records: 9,
            truncated_bytes: 0
        }
    );
    assert_eq!(
        replay(dir.path(), 4).unwrap().0,
        (4..=9).collect::<Vec<_>>()
    );
    // Past the end: nothing applied.
    assert_eq!(replay(dir.path(), 10).unwrap().0, Vec::<u64>::new());
}

#[test]
fn a_torn_tail_at_every_offset_is_cut_and_synced() {
    let one = record_bytes();
    for cut in 1..one {
        let dir = tempfile::tempdir().unwrap();
        let full = write_segment(dir.path(), 1, 1..=3);
        let path = segment::path(dir.path(), 1);
        std::fs::write(&path, &full[..full.len() - one + cut]).unwrap();
        let (seen, replayed) = replay(dir.path(), 1).unwrap();
        assert_eq!(seen, [1, 2], "cut {cut}");
        assert_eq!(replayed.truncated_bytes, cut as u64);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len() as usize,
            full.len() - one,
            "the tail is gone from the file"
        );
        // A second replay sees a clean log.
        assert_eq!(replay(dir.path(), 1).unwrap().1.truncated_bytes, 0);
    }
}

#[test]
fn garbage_and_zeros_after_good_records_are_a_tail() {
    for garbage in [vec![0u8; 300], vec![0xa5; 300]] {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = write_segment(dir.path(), 1, 1..=3);
        bytes.extend(&garbage);
        std::fs::write(segment::path(dir.path(), 1), &bytes).unwrap();
        let (seen, replayed) = replay(dir.path(), 1).unwrap();
        assert_eq!((seen.len(), replayed.truncated_bytes), (3, 300));
    }
}

#[test]
fn damage_before_intact_records_of_the_last_segment_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = write_segment(dir.path(), 1, 1..=5);
    let one = record_bytes();
    bytes[HEADER_BYTES + one + 12] ^= 0x01; // inside record 2
    std::fs::write(segment::path(dir.path(), 1), &bytes).unwrap();
    assert!(matches!(
        replay(dir.path(), 1),
        Err(WalError::Corrupt {
            segment: 1,
            reason: "damage before intact records",
            ..
        })
    ));
}

#[test]
fn any_damage_in_a_sealed_segment_is_corrupt() {
    let one = record_bytes();
    for at in [0, 20, HEADER_BYTES + 3, HEADER_BYTES + one + 10] {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = write_segment(dir.path(), 1, 1..=3);
        write_segment(dir.path(), 4, 4..=5);
        bytes[at] ^= 0x10;
        std::fs::write(segment::path(dir.path(), 1), &bytes).unwrap();
        assert!(
            matches!(
                replay(dir.path(), 1),
                Err(WalError::Corrupt { segment: 1, .. })
            ),
            "byte {at}"
        );
    }
    // A sealed segment that ends in a partial record.
    let dir = tempfile::tempdir().unwrap();
    let bytes = write_segment(dir.path(), 1, 1..=3);
    write_segment(dir.path(), 4, 4..=5);
    std::fs::write(segment::path(dir.path(), 1), &bytes[..bytes.len() - 3]).unwrap();
    assert!(matches!(
        replay(dir.path(), 1),
        Err(WalError::Corrupt { segment: 1, .. })
    ));
}

#[test]
fn duplicated_or_decreasing_sequences_are_corrupt() {
    for seqs in [vec![1, 2, 2], vec![1, 3, 2], vec![1, 2, 4]] {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 1, seqs.clone());
        assert!(
            matches!(replay(dir.path(), 1), Err(WalError::Corrupt { .. })),
            "{seqs:?}"
        );
    }
    // Across segments.
    let dir = tempfile::tempdir().unwrap();
    write_segment(dir.path(), 1, 1..=3);
    write_segment(dir.path(), 3, 3..=4);
    assert!(matches!(
        replay(dir.path(), 1),
        Err(WalError::Corrupt { segment: 3, .. })
    ));
}

#[test]
fn a_missing_segment_is_a_gap_unless_wholly_below_from_seq() {
    let dir = tempfile::tempdir().unwrap();
    write_segment(dir.path(), 1, 1..=3);
    write_segment(dir.path(), 7, 7..=9);
    assert!(matches!(
        replay(dir.path(), 1),
        Err(WalError::Gap {
            expected: 4,
            found: 7
        })
    ));
    assert!(matches!(
        replay(dir.path(), 5),
        Err(WalError::Gap {
            expected: 5,
            found: 7
        })
    ));
    // Records 4..=6 were below the checkpoint: the gap is harmless.
    assert_eq!(replay(dir.path(), 7).unwrap().0, [7, 8, 9]);
    // A log that starts above from_seq.
    let dir = tempfile::tempdir().unwrap();
    write_segment(dir.path(), 10, 10..=11);
    assert!(matches!(
        replay(dir.path(), 4),
        Err(WalError::Gap {
            expected: 4,
            found: 10
        })
    ));
}

#[test]
fn old_segments_are_skipped_and_header_mismatches_are_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    // A redundant segment with damaged records: never read, only its header.
    let mut bytes = write_segment(dir.path(), 1, 1..=3);
    bytes[HEADER_BYTES + 2] ^= 1;
    std::fs::write(segment::path(dir.path(), 1), &bytes).unwrap();
    write_segment(dir.path(), 4, 4..=6);
    assert_eq!(replay(dir.path(), 5).unwrap().0, [5, 6]);

    let dir = tempfile::tempdir().unwrap();
    let bytes = write_segment(dir.path(), 1, 1..=2);
    std::fs::write(segment::path(dir.path(), 9), &bytes).unwrap();
    std::fs::remove_file(segment::path(dir.path(), 1)).unwrap();
    assert!(matches!(
        replay(dir.path(), 1),
        Err(WalError::Corrupt {
            reason: "header names another sequence",
            ..
        })
    ));
}

#[test]
fn a_torn_header_deletes_the_last_segment_only() {
    let dir = tempfile::tempdir().unwrap();
    write_segment(dir.path(), 1, 1..=3);
    std::fs::write(segment::path(dir.path(), 4), &encode_header(4)[..10]).unwrap();
    assert_eq!(replay(dir.path(), 1).unwrap().0, [1, 2, 3]);
    assert!(!segment::path(dir.path(), 4).exists());

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(segment::path(dir.path(), 1), [0u8; 10]).unwrap();
    write_segment(dir.path(), 4, 4..=5);
    assert!(matches!(
        replay(dir.path(), 1),
        Err(WalError::Corrupt { segment: 1, .. })
    ));
}

#[test]
fn empty_and_absent_directories_replay_nothing() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(replay(dir.path(), 1).unwrap().1, Replayed::default());
    std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
    assert_eq!(replay(dir.path(), 1).unwrap().1, Replayed::default());
    assert_eq!(
        replay(&dir.path().join("missing"), 1).unwrap().1,
        Replayed::default()
    );
}
