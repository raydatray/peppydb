use std::fs;
use std::path::PathBuf;

use super::*;

const PAGE: usize = 128;

fn temp_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.bf");
    (dir, path)
}

#[test]
fn create_writes_a_superblock_page_and_refuses_to_clobber() {
    let (_dir, path) = temp_path();
    let store = FileStore::create(&path, superblock(PAGE)).unwrap();
    assert_eq!(store.slots(), 0);
    assert_eq!(fs::metadata(&path).unwrap().len(), PAGE as u64);

    let bytes = fs::read(&path).unwrap();
    assert_eq!(Superblock::decode(&bytes), Ok(superblock(PAGE)));

    assert!(matches!(
        FileStore::create(&path, superblock(PAGE)),
        Err(StoreError::Io { op: "create", .. })
    ));
}

#[test]
fn reopening_sees_written_pages_and_treats_zero_slots_as_live() {
    let (_dir, path) = temp_path();
    {
        let mut store = FileStore::create(&path, superblock(PAGE)).unwrap();
        let ids: Vec<_> = (0..4).map(|_| store.alloc().unwrap()).collect();
        for (fill, &id) in (1u8..).zip(&ids) {
            store.write(id, &page_image(PAGE, fill)).unwrap();
        }
        store.free(ids[2]).unwrap();
        store.sync().unwrap();
    }
    assert_eq!(fs::metadata(&path).unwrap().len(), 5 * PAGE as u64);

    let mut store = FileStore::open(&path).unwrap();
    assert_eq!(*store.superblock(), superblock(PAGE));
    assert_eq!(store.slots(), 4);

    let mut buf = vec![0; PAGE];
    store.read(leaf(1), &mut buf).unwrap();
    assert_eq!(buf, page_image(PAGE, 2));
    store.read(leaf(2), &mut buf).unwrap();
    assert!(buf.iter().all(|&byte| byte == 0));

    // No scan on open: the zero slot is live until a caller frees it.
    assert_eq!(store.alloc().unwrap(), leaf(4));
    store.free(leaf(2)).unwrap();
    assert_eq!(
        store.write(leaf(2), &buf),
        Err(StoreError::FreedSlot(leaf(2)))
    );
    assert_eq!(store.alloc().unwrap(), leaf(2));
    assert_eq!(store.alloc().unwrap(), leaf(5));
}

#[test]
fn alloc_extends_the_file_with_a_zeroed_page() {
    let (_dir, path) = temp_path();
    let mut store = FileStore::create(&path, superblock(PAGE)).unwrap();
    store.alloc().unwrap();
    store.alloc().unwrap();
    drop(store);

    let bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.len(), 3 * PAGE);
    assert!(bytes[PAGE..].iter().all(|&byte| byte == 0));
}

#[test]
fn short_and_ragged_files_are_rejected() {
    let (_dir, path) = temp_path();
    fs::write(&path, b"short").unwrap();
    assert_eq!(
        FileStore::open(&path).err(),
        Some(StoreError::InvalidSuperblock("too short"))
    );

    let mut page = vec![0; PAGE];
    superblock(PAGE).encode(&mut page).unwrap();
    let mut ragged = page.clone();
    ragged.extend_from_slice(&[0; PAGE / 2]);
    fs::write(&path, &ragged).unwrap();
    assert_eq!(
        FileStore::open(&path).err(),
        Some(StoreError::TruncatedFile)
    );

    fs::write(&path, &page[..ENCODED_BYTES + 1]).unwrap();
    assert_eq!(
        FileStore::open(&path).err(),
        Some(StoreError::TruncatedFile)
    );
}

#[test]
fn foreign_superblocks_are_rejected() {
    let (_dir, path) = temp_path();
    let mut page = vec![0; PAGE];
    superblock(PAGE).encode(&mut page).unwrap();
    page[0] = b'x';
    fs::write(&path, &page).unwrap();
    assert_eq!(
        FileStore::open(&path).err(),
        Some(StoreError::InvalidSuperblock("bad magic"))
    );

    assert!(matches!(
        FileStore::open(path.with_extension("missing")).err(),
        Some(StoreError::Io { op: "open", .. })
    ));
}

#[test]
fn the_page_size_comes_from_the_file_not_the_caller() {
    let (_dir, path) = temp_path();
    FileStore::create(&path, superblock(256)).unwrap();
    let store = FileStore::open(&path).unwrap();
    assert_eq!(store.page_size(), 256);
}
