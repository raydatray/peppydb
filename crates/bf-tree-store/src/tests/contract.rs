//! Observable behavior every backend must share. Each backend registers a
//! factory; the same scenarios run against all of them.

use super::*;

const PAGE: usize = 128;

type Factory = fn() -> Box<dyn PageStore>;

fn mem() -> Box<dyn PageStore> {
    Box::new(MemStore::new(superblock(PAGE)).unwrap())
}

/// The temporary directory lives as long as the store; the store never
/// touches the path again after `create`.
fn file() -> Box<dyn PageStore> {
    struct Owned {
        store: FileStore,
        _dir: tempfile::TempDir,
    }

    impl PageStore for Owned {
        fn page_size(&self) -> usize {
            self.store.page_size()
        }
        fn slots(&self) -> u64 {
            self.store.slots()
        }
        fn alloc(&mut self) -> Result<LeafId, StoreError> {
            self.store.alloc()
        }
        fn free(&mut self, id: LeafId) -> Result<(), StoreError> {
            self.store.free(id)
        }
        fn read(&self, id: LeafId, buf: &mut [u8]) -> Result<(), StoreError> {
            self.store.read(id, buf)
        }
        fn write(&mut self, id: LeafId, buf: &[u8]) -> Result<(), StoreError> {
            self.store.write(id, buf)
        }
        fn sync(&mut self) -> Result<(), StoreError> {
            self.store.sync()
        }
        fn superblock(&self) -> &Superblock {
            self.store.superblock()
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::create(dir.path().join("store.bf"), superblock(PAGE)).unwrap();
    Box::new(Owned { store, _dir: dir })
}

fn backends() -> Vec<(&'static str, Factory)> {
    vec![("mem", mem as Factory), ("file", file as Factory)]
}

fn for_each_backend(scenario: impl Fn(&mut dyn PageStore)) {
    for (name, factory) in backends() {
        let mut store = factory();
        eprintln!("backend {name}");
        scenario(store.as_mut());
    }
}

#[test]
fn new_stores_are_empty_and_describe_themselves() {
    for_each_backend(|store| {
        assert_eq!(store.slots(), 0);
        assert_eq!(store.page_size(), PAGE);
        assert_eq!(*store.superblock(), superblock(PAGE));
    });
}

#[test]
fn alloc_hands_out_dense_zeroed_slots() {
    for_each_backend(|store| {
        let ids: Vec<_> = (0..3).map(|_| store.alloc().unwrap()).collect();
        assert_eq!(ids, [leaf(0), leaf(1), leaf(2)]);
        assert_eq!(store.slots(), 3);

        let mut buf = vec![1; PAGE];
        store.read(leaf(1), &mut buf).unwrap();
        assert!(buf.iter().all(|&byte| byte == 0));
    });
}

#[test]
fn writes_round_trip_independently_per_slot() {
    for_each_backend(|store| {
        let a = store.alloc().unwrap();
        let b = store.alloc().unwrap();
        store.write(a, &page_image(PAGE, 0xaa)).unwrap();
        store.write(b, &page_image(PAGE, 0xbb)).unwrap();
        store.sync().unwrap();

        let mut buf = vec![0; PAGE];
        store.read(a, &mut buf).unwrap();
        assert_eq!(buf, page_image(PAGE, 0xaa));
        store.read(b, &mut buf).unwrap();
        assert_eq!(buf, page_image(PAGE, 0xbb));

        store.write(a, &page_image(PAGE, 0x11)).unwrap();
        store.read(a, &mut buf).unwrap();
        assert_eq!(buf, page_image(PAGE, 0x11));
        store.read(b, &mut buf).unwrap();
        assert_eq!(buf, page_image(PAGE, 0xbb));
    });
}

#[test]
fn freed_slots_are_zeroed_and_reused_before_growth() {
    for_each_backend(|store| {
        let ids: Vec<_> = (0..3).map(|_| store.alloc().unwrap()).collect();
        for &id in &ids {
            store.write(id, &page_image(PAGE, 0xcc)).unwrap();
        }

        store.free(ids[1]).unwrap();
        store.free(ids[0]).unwrap();
        assert_eq!(store.slots(), 3, "freeing does not shrink the slot count");

        let mut buf = vec![1; PAGE];
        store.read(ids[1], &mut buf).unwrap();
        assert!(
            buf.iter().all(|&byte| byte == 0),
            "freed slot reads as zeros"
        );

        // Last freed, first reused.
        assert_eq!(store.alloc().unwrap(), ids[0]);
        assert_eq!(store.alloc().unwrap(), ids[1]);
        assert_eq!(store.alloc().unwrap(), leaf(3));
        assert_eq!(store.slots(), 4);
    });
}

#[test]
fn freed_slots_reject_writes_and_double_frees() {
    for_each_backend(|store| {
        let id = store.alloc().unwrap();
        store.free(id).unwrap();

        assert_eq!(
            store.write(id, &page_image(PAGE, 1)),
            Err(StoreError::FreedSlot(id))
        );
        assert_eq!(store.free(id), Err(StoreError::FreedSlot(id)));
    });
}

#[test]
fn unknown_slots_are_rejected() {
    for_each_backend(|store| {
        store.alloc().unwrap();
        let mut buf = vec![0; PAGE];
        let missing = leaf(1);
        let far = leaf(u64::MAX - 1);

        assert_eq!(
            store.read(missing, &mut buf),
            Err(StoreError::UnknownSlot(missing))
        );
        assert_eq!(
            store.write(missing, &buf),
            Err(StoreError::UnknownSlot(missing))
        );
        assert_eq!(store.free(missing), Err(StoreError::UnknownSlot(missing)));
        assert_eq!(store.read(far, &mut buf), Err(StoreError::UnknownSlot(far)));
    });
}

#[test]
fn buffers_must_be_exactly_one_page() {
    for_each_backend(|store| {
        let id = store.alloc().unwrap();
        for len in [0, PAGE - 1, PAGE + 1, 2 * PAGE] {
            let expected = Err(StoreError::BufferLength {
                expected: PAGE,
                actual: len,
            });
            let mut buf = vec![0; len];
            assert_eq!(store.read(id, &mut buf), expected);
            assert_eq!(store.write(id, &buf), expected);
        }
    });
}
