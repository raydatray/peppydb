//! Open, recovery, and reads.

use crate::*;
use engine_wal::{Wal, WalOptions};
use std::ops::ControlFlow;

fn small() -> Options {
    Options {
        tree: TreeConfig {
            circular_buffer_bytes: 64 << 10,
            ..TreeConfig {
                leaf_capacity: 256,
                inner_capacity: 128,
                max_key_len: 8,
                max_value_len: 16,
                ..TreeConfig::default()
            }
        },
        ..Options::default()
    }
}

#[test]
fn create_close_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    assert_eq!(db.applied_seq(), 0);
    assert!(dir.path().join("tree.bf").exists());
    assert!(dir.path().join("wal").is_dir());
    db.close().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), None);
    assert_eq!(db.range(None, None).unwrap().count(), 0);
}

#[test]
fn a_wal_written_beside_the_tree_is_replayed() {
    let dir = tempfile::tempdir().unwrap();
    Db::open(dir.path(), small()).unwrap().close().unwrap();
    // Write records straight into the WAL, as a crashed engine would leave.
    let wal_dir = dir.path().join("wal");
    let next = Wal::replay(&wal_dir, 1, |_| Ok::<_, WalError>(()))
        .unwrap()
        .last_seq
        .map_or(1, |last| last + 1);
    let wal = Wal::open(&wal_dir, WalOptions::default(), next).unwrap();
    for i in 0..50u8 {
        wal.append(engine_wal::Op::Put {
            key: &[i],
            value: &[i; 4],
        })
        .unwrap();
    }
    wal.append_batch(&[
        engine_wal::Op::Delete { key: &[3] },
        engine_wal::Op::Put {
            key: b"batch",
            value: b"x",
        },
    ])
    .unwrap();
    wal.close().unwrap();

    let db = Db::open(dir.path(), small()).unwrap();
    assert_eq!(db.applied_seq(), next + 50);
    assert_eq!(db.get(&[7]).unwrap(), Some(vec![7; 4]));
    assert_eq!(db.get(&[3]).unwrap(), None);
    assert_eq!(db.get(b"batch").unwrap(), Some(b"x".to_vec()));
    let mut keys = 0;
    db.scan_with(None, None, |_, _| {
        keys += 1;
        ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(keys, 50);
}

#[test]
fn a_reopen_with_other_limits_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    Db::open(dir.path(), small()).unwrap().close().unwrap();
    let mut other = small();
    other.tree.max_key_len = 9;
    assert!(matches!(
        Db::open(dir.path(), other),
        Err(Error::Options(_))
    ));
    let mut bad = small();
    bad.segment_bytes = 10;
    assert!(matches!(Db::open(dir.path(), bad), Err(Error::Options(_))));
}

#[test]
fn a_second_open_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    assert!(matches!(Db::open(dir.path(), small()), Err(Error::Locked)));
    drop(db);
    Db::open(dir.path(), small()).unwrap();
}

#[test]
fn a_stale_wal_with_a_gap_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    Db::open(dir.path(), small()).unwrap().close().unwrap();
    let wal_dir = dir.path().join("wal");
    for entry in std::fs::read_dir(&wal_dir).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    // A log that starts above the tree's applied sequence plus one.
    let wal = Wal::open(&wal_dir, WalOptions::default(), 5).unwrap();
    wal.append(engine_wal::Op::Put {
        key: b"k",
        value: b"v",
    })
    .unwrap();
    wal.close().unwrap();
    assert!(matches!(
        Db::open(dir.path(), small()),
        Err(Error::Wal(WalError::Gap {
            expected: 1,
            found: 5
        }))
    ));
}
