//! Direct I/O (`Io::Direct`, plan 0023). The Linux parts run in the
//! container (`tools/linux-run`), with data in `PEPPYDB_BENCH_DIR` when set:
//! `tmpfs` rejects `O_DIRECT`, so a directory that cannot take it is a skip,
//! not a pass.

use super::*;

const PAGE: usize = 4096;

#[cfg(not(target_os = "linux"))]
#[test]
fn direct_io_is_refused_off_linux() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        FileStore::create_with(dir.path().join("s.bf"), superblock(PAGE), Io::Direct).err(),
        Some(StoreError::DirectIo("direct I/O requires Linux"))
    );
    let store = FileStore::create(dir.path().join("c.bf"), superblock(PAGE)).unwrap();
    assert_eq!(store.io(), Io::Cached);
    drop(store);
    assert_eq!(
        FileStore::open_with(dir.path().join("c.bf"), Io::Direct).err(),
        Some(StoreError::DirectIo("direct I/O requires Linux"))
    );
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::unix::fs::FileExt;
    use std::path::PathBuf;

    /// A directory for direct-I/O files, or `None` (with a message) if this
    /// file system rejects `O_DIRECT`.
    fn direct_dir() -> Option<(tempfile::TempDir, PathBuf)> {
        let dir = match std::env::var_os("PEPPYDB_BENCH_DIR") {
            Some(base) => tempfile::tempdir_in(base).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let path = dir.path().join("direct.bf");
        match FileStore::create_with(&path, superblock(PAGE), Io::Direct) {
            Ok(_) => {
                std::fs::remove_file(&path).unwrap();
                Some((dir, path))
            }
            Err(StoreError::Io {
                kind: std::io::ErrorKind::InvalidInput,
                ..
            }) => {
                eprintln!("skipped: {} rejects O_DIRECT", dir.path().display());
                None
            }
            Err(error) => panic!("{error:?}"),
        }
    }

    #[test]
    fn direct_stores_need_4096_byte_pages() {
        let Some((_dir, path)) = direct_dir() else {
            return;
        };
        assert_eq!(
            FileStore::create_with(&path, superblock(256), Io::Direct).err(),
            Some(StoreError::DirectIo("direct I/O requires 4096-byte pages"))
        );
        // A cached store with small pages refuses to reopen direct.
        FileStore::create(&path, superblock(256)).unwrap();
        assert_eq!(
            FileStore::open_with(&path, Io::Direct).err(),
            Some(StoreError::DirectIo("direct I/O requires 4096-byte pages"))
        );
    }

    #[test]
    fn o_direct_is_in_effect() {
        let Some((_dir, path)) = direct_dir() else {
            return;
        };
        let store = FileStore::create_with(&path, superblock(PAGE), Io::Direct).unwrap();
        // A read into a buffer at an odd address fails: the kernel does not
        // copy through the page cache for this handle.
        let mut buf = vec![0u8; PAGE + 1];
        let error = store.file().read_exact_at(&mut buf[1..], 0).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
        // The store's own reads handle any buffer, aligned or not.
        let page_number = store.alloc().unwrap();
        store.write(page_number, &page_image(PAGE, 0x42)).unwrap();
        let mut odd = vec![0u8; PAGE + 3];
        store.read(page_number, &mut odd[3..]).unwrap();
        assert_eq!(&odd[3..], page_image(PAGE, 0x42));
    }

    #[test]
    fn a_direct_store_behaves_like_a_cached_one() {
        let Some((_dir, path)) = direct_dir() else {
            return;
        };
        let store = FileStore::create_with(&path, superblock(PAGE), Io::Direct).unwrap();
        assert_eq!(store.io(), Io::Direct);
        let pages: Vec<_> = (0..8).map(|_| store.alloc().unwrap()).collect();
        for (fill, &page_number) in (1u8..).zip(&pages) {
            store.write(page_number, &page_image(PAGE, fill)).unwrap();
        }
        store.free(pages[3]).unwrap();
        assert_eq!(store.alloc().unwrap(), pages[3], "freed pages are reused");
        store.sync().unwrap();
        let state = State {
            checkpoint_id: 1,
            applied_seq: 5,
            write_seq: 9,
        };
        store.commit(&state).unwrap();
        drop(store);

        // Reopen in both modes: the format does not depend on the mode.
        for io in [Io::Cached, Io::Direct] {
            let store = FileStore::open_with(&path, io).unwrap();
            assert_eq!(
                (store.io(), store.state(), store.page_count()),
                (io, state, 8)
            );
            let mut buf = vec![0; PAGE];
            for (fill, &page_number) in (1u8..).zip(&pages) {
                store.read(page_number, &mut buf).unwrap();
                assert_eq!(buf, page_image(PAGE, fill), "{io:?}");
            }
        }
    }
}
