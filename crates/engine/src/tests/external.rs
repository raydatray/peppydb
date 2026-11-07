//! External-log mode (plan 0022).

use std::collections::BTreeMap;

use super::small;
use crate::*;

pub(super) fn external() -> Options {
    Options {
        mode: Mode::External,
        ..small()
    }
}

fn contents(db: &Db) -> BTreeMap<Vec<u8>, Vec<u8>> {
    db.range(None, None).unwrap().map(Result::unwrap).collect()
}

fn put<'a>(key: &'a [u8], value: &'a [u8]) -> Op<'a> {
    Op::Put { key, value }
}

#[test]
fn each_mode_refuses_the_other_modes_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), external()).unwrap();
    assert!(matches!(db.put(b"a", b"1"), Err(Error::WrongMode)));
    assert!(matches!(db.delete(b"a"), Err(Error::WrongMode)));
    assert!(matches!(db.write(WriteBatch::new()), Err(Error::WrongMode)));
    // Reads, checkpoints, and snapshots work in both modes.
    assert_eq!(db.get(b"a").unwrap(), None);
    db.checkpoint().unwrap();
    db.write_snapshot(&mut Vec::new()).unwrap();
    db.close().unwrap();

    let other = tempfile::tempdir().unwrap();
    let db = Db::open(other.path(), small()).unwrap();
    assert!(matches!(
        db.apply(1, &[put(b"a", b"1")]),
        Err(Error::WrongMode)
    ));
    assert!(matches!(
        db.checkpoint_with(|_| Ok(())),
        Err(Error::WrongMode)
    ));
}

#[test]
fn entries_apply_in_order_with_gaps_and_redeliveries_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), external()).unwrap();
    assert!(!dir.path().join("wal").exists(), "no WAL in external mode");
    assert_eq!(db.apply(1, &[put(b"a", b"1")]).unwrap(), Applied::Applied);
    assert_eq!(
        db.apply(5, &[]).unwrap(),
        Applied::Applied,
        "an empty entry"
    );
    assert_eq!(
        db.apply(
            9,
            &[put(b"b", b"2"), Op::Delete { key: b"a" }, put(b"a", b"3")]
        )
        .unwrap(),
        Applied::Applied
    );
    assert_eq!(db.applied_seq(), 9);
    // Redeliveries change nothing.
    for seq in [1, 5, 9] {
        assert_eq!(
            db.apply(seq, &[put(b"a", b"old")]).unwrap(),
            Applied::AlreadyApplied
        );
    }
    assert_eq!(db.get(b"a").unwrap(), Some(b"3".to_vec()), "last per key");
    assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn an_entry_past_the_limits_is_rejected_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), external()).unwrap();
    assert!(matches!(
        db.apply(1, &[put(b"a", b"1"), put(&[7; 9], b"v")]),
        Err(Error::Tree(TreeError::KeyTooLarge))
    ));
    assert_eq!(db.get(b"a").unwrap(), None);
    assert_eq!(db.applied_seq(), 0);
    // The same sequence can then be applied correctly.
    assert_eq!(db.apply(1, &[put(b"a", b"1")]).unwrap(), Applied::Applied);
}

#[test]
fn a_failing_hook_aborts_the_checkpoint_and_returns_the_owners_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), external()).unwrap();
    db.apply(1, &[put(b"a", b"1")]).unwrap();
    let committed = db.checkpoint_with(|seq| {
        assert!(seq >= 1);
        Ok(())
    });
    assert_eq!(committed.unwrap().applied_seq, 1);
    db.apply(2, &[put(b"b", b"2")]).unwrap();
    let error = db
        .checkpoint_with(|_| Err("the owner's log is down".into()))
        .unwrap_err();
    assert!(
        matches!(&error, Error::Hook(inner) if inner.to_string() == "the owner's log is down"),
        "{error:?}"
    );
    drop(db);
    // The failed checkpoint committed nothing: a reopen is at 1.
    let db = Db::open(dir.path(), external()).unwrap();
    assert_eq!(db.applied_seq(), 1);
    assert_eq!(db.get(b"b").unwrap(), None);
}

#[test]
fn a_reopen_resumes_at_the_committed_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), external()).unwrap();
    for seq in 1..=50u64 {
        db.apply(seq, &[put(&seq.to_be_bytes(), b"v")]).unwrap();
        if seq == 30 {
            db.checkpoint().unwrap();
        }
    }
    drop(db); // a crash for an external database: no WAL to drain
    let db = Db::open(dir.path(), external()).unwrap();
    assert_eq!(db.applied_seq(), 30);
    assert_eq!(contents(&db).len(), 30);
    // The owner redelivers what it retained, from a little below the
    // commit: entries at or below 30 are skipped.
    for seq in 25..=50u64 {
        let applied = db.apply(seq, &[put(&seq.to_be_bytes(), b"v")]).unwrap();
        let expected = if seq <= 30 {
            Applied::AlreadyApplied
        } else {
            Applied::Applied
        };
        assert_eq!(applied, expected, "seq {seq}");
    }
    assert_eq!(contents(&db).len(), 50);
    db.close().unwrap();
    let db = Db::open(dir.path(), external()).unwrap();
    assert_eq!(db.applied_seq(), 50, "close checkpoints");
}

#[test]
fn an_external_open_over_a_standalone_wal_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    db.put(b"a", b"1").unwrap();
    db.close().unwrap();
    assert!(matches!(
        Db::open(dir.path(), external()),
        Err(Error::Options("a standalone WAL exists"))
    ));
    // Converting: after close the WAL replays nothing; deleting it is the
    // owner's explicit choice.
    std::fs::remove_dir_all(dir.path().join("wal")).unwrap();
    let db = Db::open(dir.path(), external()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
}

/// A leader in external mode with entries 1..=n, checkpointed.
fn leader(dir: &std::path::Path, n: u64) -> Db {
    let db = Db::open(dir, external()).unwrap();
    for seq in 1..=n {
        db.apply(seq, &[put(&seq.to_be_bytes(), &[seq as u8; 4])])
            .unwrap();
    }
    db.checkpoint().unwrap();
    db
}

fn snapshot(db: &Db) -> Vec<u8> {
    let mut bytes = Vec::new();
    db.write_snapshot(&mut bytes).unwrap();
    bytes
}

#[test]
fn installing_a_snapshot_replaces_the_state() {
    let leader_dir = tempfile::tempdir().unwrap();
    let leader = leader(leader_dir.path(), 200);
    let bytes = snapshot(&leader);
    let dir = tempfile::tempdir().unwrap();
    let follower = Db::open(dir.path(), external()).unwrap();
    follower.apply(1, &[put(b"stale", b"x")]).unwrap();
    let follower = follower.install_snapshot(&mut bytes.as_slice()).unwrap();
    assert_eq!(follower.applied_seq(), 200);
    assert_eq!(contents(&follower), contents(&leader));
    assert!(!dir.path().join(PARTIAL).exists());
    // The lock was kept: a second handle cannot open the database.
    assert!(matches!(
        Db::open(dir.path(), external()),
        Err(Error::Locked)
    ));
    // The follower continues from the snapshot.
    follower.apply(201, &[put(b"next", b"y")]).unwrap();
    follower.close().unwrap();
    let follower = Db::open(dir.path(), external()).unwrap();
    assert_eq!(follower.applied_seq(), 201);
}

#[test]
fn a_bad_snapshot_is_rejected_and_the_old_state_reopens() {
    let leader_dir = tempfile::tempdir().unwrap();
    let leader = leader(leader_dir.path(), 100);
    let good = snapshot(&leader);
    let mut flipped = good.clone();
    flipped[20] ^= 0xff; // in both superblock copies
    flipped[4096 + 20] ^= 0xff;
    let other_dir = tempfile::tempdir().unwrap();
    let other_limits = Options {
        tree: TreeConfig {
            max_key_len: 9,
            ..external().tree
        },
        ..external()
    };
    let other = Db::open(other_dir.path(), other_limits).unwrap();
    other.apply(1, &[put(b"z", b"z")]).unwrap();
    other.checkpoint().unwrap();
    let mismatched = snapshot(&other);
    let cases: [(&str, &[u8]); 3] = [
        ("truncated", &good[..good.len() - 100]),
        ("flipped superblocks", &flipped),
        ("other limits", &mismatched),
    ];
    for (name, bytes) in cases {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), external()).unwrap();
        db.apply(1, &[put(b"keep", b"me")]).unwrap();
        db.checkpoint().unwrap();
        let mut reader = bytes;
        let error = db.install_snapshot(&mut reader).unwrap_err();
        assert!(
            matches!(error, Error::Tree(_) | Error::Options(_)),
            "{name}: {error:?}"
        );
        assert!(!dir.path().join(PARTIAL).exists(), "{name}");
        let db = Db::open(dir.path(), external()).unwrap();
        assert_eq!(db.get(b"keep").unwrap(), Some(b"me".to_vec()), "{name}");
    }
}

#[test]
fn a_leftover_partial_snapshot_is_removed_by_open() {
    let dir = tempfile::tempdir().unwrap();
    Db::open(dir.path(), external()).unwrap().close().unwrap();
    std::fs::write(dir.path().join(PARTIAL), b"half a snapshot").unwrap();
    let db = Db::open(dir.path(), external()).unwrap();
    assert!(!dir.path().join(PARTIAL).exists());
    drop(db);
}

#[test]
fn standalone_install_is_wrong_mode() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    assert!(matches!(
        db.install_snapshot(&mut [].as_slice()),
        Err(Error::WrongMode)
    ));
}
