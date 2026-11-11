use std::fs;
use std::path::PathBuf;

use super::*;

const PAGE: usize = 128;
const AREA: usize = PAGES_OFFSET as usize;

fn temp_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.bf");
    (dir, path)
}

fn state(checkpoint_id: u64) -> State {
    State {
        checkpoint_id,
        applied_seq: checkpoint_id + 100,
        write_seq: checkpoint_id + 200,
    }
}

/// One encoded superblock copy.
fn copy(state: &State) -> Vec<u8> {
    let mut block = vec![0; COPY_BYTES];
    superblock(PAGE).encode(state, &mut block).unwrap();
    block
}

#[test]
fn create_writes_both_superblock_copies_and_refuses_to_clobber() {
    let (_dir, path) = temp_path();
    let store = FileStore::create(&path, superblock(PAGE)).unwrap();
    assert_eq!(store.page_count(), 0);
    assert_eq!(store.state(), State::default());
    assert_eq!(fs::metadata(&path).unwrap().len(), PAGES_OFFSET);

    let bytes = fs::read(&path).unwrap();
    for block in bytes.chunks(COPY_BYTES) {
        assert_eq!(
            Superblock::decode(block),
            Ok((superblock(PAGE), State::default()))
        );
    }

    assert!(matches!(
        FileStore::create(&path, superblock(PAGE)),
        Err(StoreError::Io { op: "create", .. })
    ));
}

#[test]
fn reopening_sees_written_pages_and_treats_freed_pages_as_live() {
    let (_dir, path) = temp_path();
    {
        let store = FileStore::create(&path, superblock(PAGE)).unwrap();
        let ids: Vec<_> = (0..4).map(|_| store.alloc().unwrap()).collect();
        for (fill, &id) in (1u8..).zip(&ids) {
            store.write(id, &page_image(PAGE, fill)).unwrap();
        }
        store.free(ids[2]).unwrap();
        store.sync().unwrap();
    }
    assert_eq!(fs::metadata(&path).unwrap().len(), (AREA + 4 * PAGE) as u64);

    let store = FileStore::open(&path).unwrap();
    assert_eq!(*store.superblock(), superblock(PAGE));
    assert_eq!(store.page_count(), 4);

    let mut buf = vec![0; PAGE];
    store.read(page(1), &mut buf).unwrap();
    assert_eq!(buf, page_image(PAGE, 2));
    store.read(page(2), &mut buf).unwrap();
    assert_eq!(buf, page_image(PAGE, 3), "freeing kept the bytes");

    // No scan on open: the freed page is live until a caller frees it.
    assert_eq!(store.alloc().unwrap(), page(4));
    store.free(page(2)).unwrap();
    assert_eq!(
        store.write(page(2), &buf),
        Err(StoreError::FreedPage(page(2)))
    );
    assert_eq!(store.alloc().unwrap(), page(2));
    assert_eq!(store.alloc().unwrap(), page(5));
}

#[test]
fn alloc_extends_the_file_after_the_superblock_area() {
    let (_dir, path) = temp_path();
    let store = FileStore::create(&path, superblock(PAGE)).unwrap();
    store.alloc().unwrap();
    let id = store.alloc().unwrap();
    store.write(id, &page_image(PAGE, 9)).unwrap();
    drop(store);

    let bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.len(), AREA + 2 * PAGE);
    assert!(bytes[AREA..AREA + PAGE].iter().all(|&byte| byte == 0));
    assert_eq!(&bytes[AREA + PAGE..], page_image(PAGE, 9));
}

#[test]
fn commits_alternate_copies_and_survive_reopen() {
    let (_dir, path) = temp_path();
    let store = FileStore::create(&path, superblock(PAGE)).unwrap();
    for id in 1..=3 {
        store.commit(&state(id)).unwrap();
        assert_eq!(store.state(), state(id));
        let bytes = fs::read(&path).unwrap();
        let written = (id % 2) as usize * COPY_BYTES;
        assert_eq!(&bytes[written..written + COPY_BYTES], copy(&state(id)));
    }
    drop(store);
    assert_eq!(FileStore::open(&path).unwrap().state(), state(3));
}

#[test]
fn a_torn_commit_reopens_at_the_previous_state() {
    let (_dir, path) = temp_path();
    let store = FileStore::create(&path, superblock(PAGE)).unwrap();
    store.commit(&state(1)).unwrap();
    store.commit(&state(2)).unwrap();
    drop(store);

    // Tear copy 2 % 2 = 0 in three ways: garbage, zeros, and half written.
    let clean = fs::read(&path).unwrap();
    let mut half = copy(&state(2));
    half[28..].fill(0);
    for torn in [vec![0x5a; COPY_BYTES], vec![0; COPY_BYTES], half] {
        let mut bytes = clean.clone();
        bytes[..COPY_BYTES].copy_from_slice(&torn);
        fs::write(&path, &bytes).unwrap();
        assert_eq!(FileStore::open(&path).unwrap().state(), state(1));
    }
}

#[test]
fn a_zeroed_first_copy_opens_from_the_second_without_probing() {
    let (_dir, path) = temp_path();
    let store = FileStore::create(&path, superblock(PAGE)).unwrap();
    store.alloc().unwrap();
    drop(store);
    let mut bytes = fs::read(&path).unwrap();
    bytes[..COPY_BYTES].fill(0);
    fs::write(&path, &bytes).unwrap();
    let store = FileStore::open(&path).unwrap();
    assert_eq!(*store.superblock(), superblock(PAGE));
    assert_eq!(store.page_count(), 1);
}

#[test]
fn short_and_ragged_files_are_rejected() {
    let (_dir, path) = temp_path();
    fs::write(&path, b"short").unwrap();
    assert_eq!(
        FileStore::open(&path).err(),
        Some(StoreError::TruncatedFile)
    );

    let area = [copy(&State::default()), copy(&State::default())].concat();
    fs::write(&path, &area[..AREA - 1]).unwrap();
    assert_eq!(
        FileStore::open(&path).err(),
        Some(StoreError::TruncatedFile)
    );

    let mut ragged = area.clone();
    ragged.extend_from_slice(&[0; PAGE / 2]);
    fs::write(&path, &ragged).unwrap();
    assert_eq!(
        FileStore::open(&path).err(),
        Some(StoreError::TruncatedFile)
    );
}

#[test]
fn foreign_superblocks_are_rejected() {
    let (_dir, path) = temp_path();
    fs::write(&path, vec![b'x'; AREA]).unwrap();
    assert_eq!(FileStore::open(&path).err(), Some(StoreError::NoSuperblock));

    let mut other = vec![0; COPY_BYTES];
    Superblock {
        max_key_len: 9,
        ..superblock(PAGE)
    }
    .encode(&State::default(), &mut other)
    .unwrap();
    fs::write(&path, [copy(&State::default()), other].concat()).unwrap();
    assert!(matches!(
        FileStore::open(&path).err(),
        Some(StoreError::InvalidSuperblock(_))
    ));

    assert!(matches!(
        FileStore::open(path.with_extension("missing")).err(),
        Some(StoreError::Io { op: "open", .. })
    ));
}
