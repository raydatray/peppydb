//! The `fault-injection` feature (plan 0026): `Db::fail_next_wal_sync`.

use crate::{Db, Error, Options};

#[test]
fn an_injected_sync_failure_fails_the_waiting_write_and_every_later_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), Options::default()).unwrap();
    db.put(b"before", b"1").unwrap();

    db.fail_next_wal_sync();
    assert!(matches!(db.put(b"failed", b"2"), Err(Error::Wal(_))));
    assert!(matches!(db.put(b"later", b"3"), Err(Error::Wal(_))));
    drop(db);

    // The acknowledged write is durable; a reopen accepts writes again.
    let db = Db::open(dir.path(), Options::default()).unwrap();
    assert_eq!(db.get(b"before").unwrap().as_deref(), Some(&b"1"[..]));
    db.put(b"after", b"4").unwrap();
    db.close().unwrap();
}
