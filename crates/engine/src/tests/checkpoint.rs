//! Checkpoints on policy, WAL truncation, and snapshots.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::small;
use crate::*;

fn contents(db: &Db) -> BTreeMap<Vec<u8>, Vec<u8>> {
    db.range(None, None).unwrap().map(Result::unwrap).collect()
}

fn segments(db: &Db) -> usize {
    std::fs::read_dir(db.path().join("wal")).unwrap().count()
}

fn committed(db: &Db) -> u64 {
    db.shared.tree.committed().checkpoint_id
}

/// Wait up to five seconds for `done`.
fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn a_manual_checkpoint_commits_and_truncates_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let options = Options {
        segment_bytes: 4096,
        ..small()
    };
    let db = Db::open(dir.path(), options).unwrap();
    for i in 0..2000u32 {
        db.put(&i.to_be_bytes(), &[1; 16]).unwrap();
    }
    assert!(segments(&db) > 3);
    let checkpoint = db.checkpoint().unwrap();
    assert_eq!(checkpoint.applied_seq, 2000);
    assert_eq!(segments(&db), 1, "only the active segment remains");
    db.close().unwrap();
    let db = Db::open(dir.path(), options).unwrap();
    assert_eq!(contents(&db).len(), 2000);
}

#[test]
fn the_byte_trigger_checkpoints_in_the_background() {
    let dir = tempfile::tempdir().unwrap();
    let options = Options {
        segment_bytes: 4096,
        checkpoint_wal_bytes: 16 << 10,
        ..small()
    };
    let db = Db::open(dir.path(), options).unwrap();
    let before = committed(&db);
    for i in 0..3000u32 {
        db.put(&i.to_be_bytes(), &[2; 16]).unwrap();
    }
    assert!(
        eventually(|| committed(&db) > before + 2),
        "several checkpoints ran"
    );
    assert!(
        eventually(|| segments(&db) < 10),
        "old segments were deleted"
    );
    db.close().unwrap();
}

#[test]
fn the_interval_trigger_fires_only_when_something_new_arrived() {
    let dir = tempfile::tempdir().unwrap();
    let options = Options {
        checkpoint_interval: Duration::from_millis(30),
        ..small()
    };
    let db = Db::open(dir.path(), options).unwrap();
    let start = committed(&db);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(committed(&db), start, "nothing new: no checkpoint");
    db.put(b"k", b"v").unwrap();
    assert!(eventually(|| db.shared.tree.committed().applied_seq == 1));
    let after = committed(&db);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(committed(&db), after, "idle again");
    db.close().unwrap();
}

#[test]
fn manual_and_background_checkpoints_race_safely() {
    let dir = tempfile::tempdir().unwrap();
    let options = Options {
        segment_bytes: 4096,
        checkpoint_wal_bytes: 8 << 10,
        checkpoint_interval: Duration::from_millis(5),
        ..small()
    };
    let db = Db::open(dir.path(), options).unwrap();
    std::thread::scope(|scope| {
        let db = &db;
        for t in 0..3u8 {
            scope.spawn(move || {
                for i in 0..800u16 {
                    db.put(&[t, (i >> 8) as u8, i as u8], &[t; 8]).unwrap();
                }
            });
        }
        scope.spawn(move || {
            for _ in 0..30 {
                db.checkpoint().unwrap();
            }
        });
    });
    let live = contents(&db);
    assert_eq!(live.len(), 2400);
    db.close().unwrap();
    let db = Db::open(dir.path(), options).unwrap();
    assert_eq!(contents(&db), live);
}

/// Writers take no lock that a checkpoint holds: the slowest write during
/// a long checkpoint is far below the checkpoint's duration.
#[test]
fn writers_are_not_stalled_by_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let options = Options {
        tree: TreeConfig::default(),
        checkpoint_wal_bytes: 1 << 40,
        checkpoint_interval: Duration::from_secs(3600),
        ..Options::default()
    };
    let db = Db::open(dir.path(), options).unwrap();
    // Many buffered leaves, so the checkpoint has much to merge.
    let mut batch = WriteBatch::new();
    for i in 0..40_000u32 {
        batch.put(&i.wrapping_mul(2_654_435_761).to_be_bytes(), &[3; 64]);
        if batch.len() == 20_000 {
            db.write(std::mem::take(&mut batch)).unwrap();
        }
    }
    let (checkpoint_took, slowest, i) = std::thread::scope(|scope| {
        let db = &db;
        let checkpointer = scope.spawn(move || {
            let started = Instant::now();
            db.checkpoint().unwrap();
            started.elapsed()
        });
        let mut slowest = Duration::ZERO;
        let mut i = 0u32;
        while !checkpointer.is_finished() {
            let started = Instant::now();
            db.put(&(1_000_000 + i).to_be_bytes(), b"w").unwrap();
            slowest = slowest.max(started.elapsed());
            i += 1;
        }
        (checkpointer.join().unwrap(), slowest, i)
    });
    assert!(
        // A writer stalled on a lock held for the whole checkpoint would
        // take about as long as the checkpoint; scheduling noise under a
        // parallel test run reaches a few percent of it (62 ms of 1.2 s
        // was seen in the Linux container), so the bound is a quarter.
        slowest * 4 < checkpoint_took,
        "slowest of {i} puts {slowest:?} vs checkpoint {checkpoint_took:?}"
    );
}

#[test]
fn a_snapshot_opens_as_a_database_at_its_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    for i in 0..500u32 {
        db.put(&i.to_be_bytes(), &[4; 8]).unwrap();
    }
    let checkpoint = db.checkpoint().unwrap();
    db.put(b"late", b"x").unwrap();
    let copy = tempfile::tempdir().unwrap();
    let mut file = std::fs::File::create(copy.path().join("tree.bf")).unwrap();
    assert_eq!(db.write_snapshot(&mut file).unwrap(), checkpoint);
    drop(file);
    let follower = Db::open(copy.path(), small()).unwrap();
    assert_eq!(follower.applied_seq(), checkpoint.applied_seq);
    assert_eq!(follower.get(b"late").unwrap(), None);
    assert_eq!(contents(&follower).len(), 500);
}
