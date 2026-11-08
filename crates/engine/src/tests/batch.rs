//! Atomic write batches.

use std::collections::BTreeMap;
use std::path::Path;

use super::small;
use crate::*;
use engine_wal::Wal;

fn contents(db: &Db) -> BTreeMap<Vec<u8>, Vec<u8>> {
    db.range(None, None).unwrap().map(Result::unwrap).collect()
}

/// The state that replaying the whole WAL into an empty map gives.
fn replay_into_map(dir: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut map = BTreeMap::new();
    Wal::replay(&dir.join("wal"), 1, |record| {
        for op in record.ops {
            match op {
                engine_wal::Op::Put { key, value } => {
                    map.insert(key.to_vec(), value.to_vec());
                }
                engine_wal::Op::Delete { key } => {
                    map.remove(key);
                }
            }
        }
        Ok::<_, WalError>(())
    })
    .unwrap();
    map
}

#[test]
fn a_batch_round_trips_with_one_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    db.put(b"gone", b"x").unwrap();
    let mut batch = WriteBatch::new();
    batch.put(b"a", b"1").put(b"b", b"2").delete(b"gone");
    assert_eq!(batch.len(), 3);
    db.write(batch).unwrap();
    assert_eq!(db.applied_seq(), 2, "one sequence for the batch");
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get(b"gone").unwrap(), None);
    db.close().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn an_empty_batch_appends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    let batch = WriteBatch::new();
    assert!(batch.is_empty());
    db.write(batch).unwrap();
    assert_eq!(db.wal.last_seq(), 0);
}

#[test]
fn an_invalid_operation_rejects_the_batch_before_logging() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    let mut batch = WriteBatch::new();
    batch.put(b"a", b"1").put(&[7; 9], b"too long");
    assert!(matches!(
        db.write(batch),
        Err(Error::Tree(TreeError::KeyTooLarge))
    ));
    assert_eq!(db.wal.last_seq(), 0, "no WAL record");
    assert_eq!(db.get(b"a").unwrap(), None);
}

#[test]
fn the_last_operation_per_key_wins() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    let mut batch = WriteBatch::new();
    batch
        .put(b"k", b"1")
        .delete(b"k")
        .put(b"k", b"3")
        .put(b"j", b"x")
        .delete(b"j");
    db.write(batch).unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"3".to_vec()));
    assert_eq!(db.get(b"j").unwrap(), None);
}

/// Batches and single writes race on four keys. The gate keeps per-key
/// order equal to sequence order, so the live state equals a replay of
/// the WAL into a map, and so does the reopened state.
#[test]
fn batches_and_single_writes_keep_the_wal_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    let keys: [&[u8]; 4] = [b"k0", b"k1", b"k2", b"k3"];
    std::thread::scope(|scope| {
        for t in 0..6u8 {
            let db = &db;
            scope.spawn(move || {
                for i in 0..300u16 {
                    let value = [t, (i >> 8) as u8, i as u8];
                    if t % 2 == 0 {
                        let mut batch = WriteBatch::new();
                        for key in keys {
                            batch.put(key, &value);
                        }
                        if i % 5 == 0 {
                            batch.delete(keys[(i % 4) as usize]);
                        }
                        db.write(batch).unwrap();
                    } else {
                        let key = keys[((i + u16::from(t)) % 4) as usize];
                        db.put(key, &value).unwrap();
                    }
                }
            });
        }
    });
    let live = contents(&db);
    db.close().unwrap();
    assert_eq!(live, replay_into_map(dir.path()));
    let db = Db::open(dir.path(), small()).unwrap();
    assert_eq!(contents(&db), live);
}
