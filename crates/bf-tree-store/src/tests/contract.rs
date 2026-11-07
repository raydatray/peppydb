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
    #[derive(Debug)]
    struct Owned {
        store: FileStore,
        _dir: tempfile::TempDir,
    }

    impl PageStore for Owned {
        fn page_size(&self) -> usize {
            self.store.page_size()
        }
        fn page_count(&self) -> u64 {
            self.store.page_count()
        }
        fn alloc(&self) -> Result<PageNumber, StoreError> {
            self.store.alloc()
        }
        fn free(&self, id: PageNumber) -> Result<(), StoreError> {
            self.store.free(id)
        }
        fn read(&self, id: PageNumber, buf: &mut [u8]) -> Result<(), StoreError> {
            self.store.read(id, buf)
        }
        fn write(&self, id: PageNumber, buf: &[u8]) -> Result<(), StoreError> {
            self.store.write(id, buf)
        }
        fn sync(&self) -> Result<(), StoreError> {
            self.store.sync()
        }
        fn superblock(&self) -> &Superblock {
            self.store.superblock()
        }
        fn state(&self) -> State {
            self.store.state()
        }
        fn commit(&self, state: &State) -> Result<(), StoreError> {
            self.store.commit(state)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::create(dir.path().join("store.bf"), superblock(PAGE)).unwrap();
    Box::new(Owned { store, _dir: dir })
}

fn backends() -> Vec<(&'static str, Factory)> {
    vec![("mem", mem as Factory), ("file", file as Factory)]
}

fn for_each_backend(scenario: impl Fn(&dyn PageStore)) {
    for (name, factory) in backends() {
        let store = factory();
        eprintln!("backend {name}");
        scenario(store.as_ref());
    }
}

#[test]
fn new_stores_are_empty_and_describe_themselves() {
    for_each_backend(|store| {
        assert_eq!(store.page_count(), 0);
        assert_eq!(store.page_size(), PAGE);
        assert_eq!(*store.superblock(), superblock(PAGE));
    });
}

#[test]
fn alloc_hands_out_dense_zeroed_pages() {
    for_each_backend(|store| {
        let ids: Vec<_> = (0..3).map(|_| store.alloc().unwrap()).collect();
        assert_eq!(ids, [page(0), page(1), page(2)]);
        assert_eq!(store.page_count(), 3);

        let mut buf = vec![1; PAGE];
        store.read(page(1), &mut buf).unwrap();
        assert!(buf.iter().all(|&byte| byte == 0));
    });
}

#[test]
fn writes_round_trip_independently_per_page() {
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
fn freed_pages_keep_their_bytes_and_are_reused_before_growth() {
    for_each_backend(|store| {
        let ids: Vec<_> = (0..3).map(|_| store.alloc().unwrap()).collect();
        for &id in &ids {
            store.write(id, &page_image(PAGE, 0xcc)).unwrap();
        }

        store.free(ids[1]).unwrap();
        store.free(ids[0]).unwrap();
        assert_eq!(
            store.page_count(),
            3,
            "freeing does not shrink the page count"
        );

        let mut buf = vec![1; PAGE];
        store.read(ids[1], &mut buf).unwrap();
        assert_eq!(buf, page_image(PAGE, 0xcc), "a freed page keeps its bytes");

        // Last freed, first reused, with its old bytes.
        assert_eq!(store.alloc().unwrap(), ids[0]);
        store.read(ids[0], &mut buf).unwrap();
        assert_eq!(buf, page_image(PAGE, 0xcc));
        assert_eq!(store.alloc().unwrap(), ids[1]);
        assert_eq!(store.alloc().unwrap(), page(3));
        assert_eq!(store.page_count(), 4);
    });
}

#[test]
fn freed_pages_reject_writes_and_double_frees() {
    for_each_backend(|store| {
        let id = store.alloc().unwrap();
        store.free(id).unwrap();

        assert_eq!(
            store.write(id, &page_image(PAGE, 1)),
            Err(StoreError::FreedPage(id))
        );
        assert_eq!(store.free(id), Err(StoreError::FreedPage(id)));
    });
}

#[test]
fn unknown_pages_are_rejected() {
    for_each_backend(|store| {
        store.alloc().unwrap();
        let mut buf = vec![0; PAGE];
        let missing = page(1);
        let far = page(u64::MAX - 1);

        assert_eq!(
            store.read(missing, &mut buf),
            Err(StoreError::UnknownPage(missing))
        );
        assert_eq!(
            store.write(missing, &buf),
            Err(StoreError::UnknownPage(missing))
        );
        assert_eq!(store.free(missing), Err(StoreError::UnknownPage(missing)));
        assert_eq!(store.read(far, &mut buf), Err(StoreError::UnknownPage(far)));
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

#[test]
fn concurrent_allocs_return_distinct_ids() {
    for_each_backend(|store| {
        let ids: Vec<PageNumber> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| (0..50).map(|_| store.alloc().unwrap()).collect::<Vec<_>>())
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        let mut raw: Vec<_> = ids.iter().map(|id| id.raw()).collect();
        raw.sort_unstable();
        raw.dedup();
        assert_eq!(raw.len(), 200);
        assert_eq!(store.page_count(), 200);
    });
}

#[test]
fn threads_write_distinct_pages_and_read_them_back() {
    for_each_backend(|store| {
        let ids: Vec<_> = (0..64).map(|_| store.alloc().unwrap()).collect();
        std::thread::scope(|scope| {
            for (thread, chunk) in ids.chunks(16).enumerate() {
                scope.spawn(move || {
                    for round in 0..20u8 {
                        for (i, &id) in chunk.iter().enumerate() {
                            let fill = (thread as u8 * 64) ^ (i as u8) ^ round;
                            store.write(id, &page_image(PAGE, fill)).unwrap();
                            let mut back = vec![0; PAGE];
                            store.read(id, &mut back).unwrap();
                            assert_eq!(back, page_image(PAGE, fill));
                        }
                    }
                });
            }
        });
    });
}

#[test]
fn commits_must_follow_the_current_checkpoint() {
    for_each_backend(|store| {
        assert_eq!(store.state(), State::default());
        let first = State {
            checkpoint_id: 1,
            applied_seq: 7,
            write_seq: 9,
        };
        store.commit(&first).unwrap();
        assert_eq!(store.state(), first);
        for id in [0, 1, 3] {
            let wrong = State {
                checkpoint_id: id,
                ..first
            };
            assert_eq!(
                store.commit(&wrong),
                Err(StoreError::CommitOrder {
                    current: 1,
                    got: id
                })
            );
        }
        assert_eq!(store.state(), first, "a rejected commit changes nothing");
    });
}
