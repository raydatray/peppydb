//! The store I/O mode (plan 0023).

use std::collections::BTreeMap;

use crate::*;

/// Default-size pages, which direct I/O needs, in a directory that can
/// take `O_DIRECT` (`PEPPYDB_BENCH_DIR` in the Linux container).
fn options(io: Io) -> Options {
    Options {
        io,
        tree: TreeConfig {
            circular_buffer_bytes: 1 << 20,
            ..TreeConfig::default()
        },
        ..Options::default()
    }
}

fn dir() -> tempfile::TempDir {
    match std::env::var_os("PEPPYDB_BENCH_DIR") {
        Some(base) => tempfile::tempdir_in(base).unwrap(),
        None => tempfile::tempdir().unwrap(),
    }
}

fn contents(db: &Db) -> BTreeMap<Vec<u8>, Vec<u8>> {
    db.range(None, None).unwrap().map(Result::unwrap).collect()
}

/// Writes, a batch, a checkpoint, and reopens in both modes. Direct runs
/// on Linux only; elsewhere it is refused.
#[test]
fn a_database_works_in_both_io_modes() {
    for io in [Io::Cached, Io::Direct] {
        let dir = dir();
        let db = match Db::open(dir.path(), options(io)) {
            Ok(db) => db,
            #[cfg(not(target_os = "linux"))]
            Err(Error::Tree(TreeError::Store(bf_tree::StoreError::DirectIo(_)))) => continue,
            Err(Error::Tree(TreeError::Store(bf_tree::StoreError::Io {
                kind: std::io::ErrorKind::InvalidInput,
                ..
            }))) => {
                eprintln!("skipped: {} rejects O_DIRECT", dir.path().display());
                continue;
            }
            Err(error) => panic!("{io:?}: {error:?}"),
        };
        for i in 0..3000u32 {
            db.put(&i.wrapping_mul(2_654_435_761).to_be_bytes(), &[i as u8; 32])
                .unwrap();
        }
        let mut batch = WriteBatch::new();
        batch.put(b"batch", b"x").delete(&0u32.to_be_bytes());
        db.write(batch).unwrap();
        db.checkpoint().unwrap();
        db.put(b"after", b"y").unwrap();
        let expected = contents(&db);
        db.close().unwrap();
        for reopen in [io, Io::Cached] {
            let db = Db::open(dir.path(), options(reopen)).unwrap();
            assert_eq!(contents(&db), expected, "{io:?} then {reopen:?}");
            db.close().unwrap();
        }
    }
}
