//! Durable single writes.

use std::collections::BTreeMap;

use super::small;
use crate::*;

fn contents(db: &Db) -> BTreeMap<Vec<u8>, Vec<u8>> {
    db.range(None, None).unwrap().map(Result::unwrap).collect()
}

#[test]
fn writes_round_trip_and_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    db.put(b"a", b"1").unwrap();
    db.put(b"b", b"2").unwrap();
    db.put(b"a", b"3").unwrap();
    db.delete(b"b").unwrap();
    db.delete(b"never").unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"3".to_vec()));
    assert_eq!(db.get(b"b").unwrap(), None);
    assert_eq!(db.applied_seq(), 5);
    // Dropping drains the WAL; the reopen replays it.
    drop(db);
    let db = Db::open(dir.path(), small()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"3".to_vec()));
    assert_eq!(db.get(b"b").unwrap(), None);
    assert_eq!(db.applied_seq(), 5);
    db.put(b"c", b"4").unwrap();
    assert_eq!(db.applied_seq(), 6, "sequences continue");
}

#[test]
fn writes_past_the_limits_are_rejected_before_logging() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    assert!(matches!(
        db.put(&[0; 9], b"v"),
        Err(Error::Tree(TreeError::KeyTooLarge))
    ));
    assert!(matches!(
        db.put(b"k", &[0; 17]),
        Err(Error::Tree(TreeError::ValueTooLarge))
    ));
    assert!(matches!(
        db.delete(b""),
        Err(Error::Tree(TreeError::EmptyKey))
    ));
    assert_eq!(db.wal.last_seq(), 0, "nothing was logged");
}

#[test]
fn concurrent_writers_match_the_oracle_live_and_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    let oracle: BTreeMap<_, _> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..6u8)
            .map(|t| {
                let db = &db;
                scope.spawn(move || {
                    let mut oracle = BTreeMap::new();
                    for i in 0..300u16 {
                        let key = vec![t, (i % 97) as u8];
                        if i % 7 == 0 {
                            db.delete(&key).unwrap();
                            oracle.remove(&key);
                        } else {
                            let value = vec![t; 1 + (i % 16) as usize];
                            db.put(&key, &value).unwrap();
                            oracle.insert(key, value);
                        }
                    }
                    oracle
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    assert_eq!(contents(&db), oracle);
    db.close().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    assert_eq!(contents(&db), oracle);
}

#[test]
fn a_closed_wal_surfaces_as_a_wal_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    db.put(b"a", b"1").unwrap();
    db.wal.close().unwrap();
    assert!(matches!(
        db.put(b"b", b"2"),
        Err(Error::Wal(WalError::Closed))
    ));
    assert_eq!(
        db.get(b"b").unwrap(),
        None,
        "the append failed under the lock"
    );
}
