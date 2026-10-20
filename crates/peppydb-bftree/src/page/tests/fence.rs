use super::*;

#[test]
fn initialize_fence_profiles_at_every_capacity_and_borrow_only_storage() {
    let profiles: [Fences<&[u8]>; 6] = [
        Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        },
        Fences {
            lower: Fence::Key(b""),
            upper: Fence::Unbounded,
        },
        Fences {
            lower: Fence::Unbounded,
            upper: Fence::Key(b""),
        },
        Fences {
            lower: Fence::Key(b"a"),
            upper: Fence::Unbounded,
        },
        Fences {
            lower: Fence::Unbounded,
            upper: Fence::Key(b"z"),
        },
        Fences {
            lower: Fence::Key(b"a"),
            upper: Fence::Key(b"z"),
        },
    ];
    for capacity in (64..=4096).step_by(64) {
        for fences in profiles {
            let mut storage = vec![0xa5; capacity + 1];
            let page = {
                let lower = match fences.lower {
                    Fence::Key(key) => Some(key.to_vec()),
                    _ => None,
                };
                let upper = match fences.upper {
                    Fence::Key(key) => Some(key.to_vec()),
                    _ => None,
                };
                Page::init_with_fences(
                    &mut storage[1..],
                    Fences {
                        lower: lower.as_deref().map_or(Fence::Unbounded, Fence::Key),
                        upper: upper.as_deref().map_or(Fence::Unbounded, Fence::Key),
                    },
                )
                .unwrap()
            };
            assert_eq!(page.fences(), Some(logical(fences)));
            assert_eq!(read_u16(page.as_bytes(), 0), 0x8002);
            assert!(page.is_empty());
            assert_eq!(page.iter().len(), 0);
            let payload: usize = [fences.lower, fences.upper]
                .iter()
                .map(|fence| match fence {
                    Fence::Key(key) => key.len(),
                    _ => 0,
                })
                .sum();
            assert_eq!(page.free_bytes(), capacity - 48 - payload);
            assert_eq!(page.reclaimable_bytes(), 0);
            for (index, fence) in [fences.lower, fences.upper].into_iter().enumerate() {
                let slot = 32 + 8 * index;
                match fence {
                    Fence::Unbounded => {
                        assert_eq!(read_u16(page.as_bytes(), slot), u16::MAX - 1 + index as u16);
                        assert_eq!(&page.as_bytes()[slot + 2..slot + 8], &[0; 6]);
                    }
                    Fence::Key(key) => {
                        let offset = usize::from(read_u16(page.as_bytes(), slot));
                        assert!(offset <= capacity);
                        assert_eq!(&page.as_bytes()[offset..offset + key.len()], key);
                        assert_eq!(read_u16(page.as_bytes(), slot + 2) as usize, key.len());
                        assert_eq!(read_u16(page.as_bytes(), slot + 4), 0);
                        assert_eq!(
                            &page.as_bytes()[slot + 6..slot + 8],
                            &[
                                key.first().copied().unwrap_or(0),
                                key.get(1).copied().unwrap_or(0)
                            ]
                        );
                    }
                }
            }
            assert_eq!(page.compact(), 0);
            assert_eq!(page.fences(), Some(logical(fences)));
            assert_eq!(storage[0], 0xa5);
        }
    }
}

#[test]
fn fence_initialization_errors_preserve_input_and_precedence() {
    let large = vec![b'z'; 4096];
    for (capacity, lower, upper, error) in [
        (
            63,
            b"z".as_slice(),
            b"a".as_slice(),
            PageError::InvalidCapacity,
        ),
        (64, b"a", b"a", PageError::InvalidFences),
        (64, b"z", b"a", PageError::InvalidFences),
        (64, b"", b"", PageError::InvalidFences),
        (64, b"a", large.as_slice(), PageError::FencesTooLarge),
        (4096, b"a", large.as_slice(), PageError::FencesTooLarge),
    ] {
        let mut storage = vec![0xa5; capacity];
        assert_eq!(
            Page::init_with_fences(
                &mut storage,
                Fences {
                    lower: Fence::Key(lower),
                    upper: Fence::Key(upper)
                }
            )
            .err(),
            Some(error)
        );
        assert_eq!(storage, vec![0xa5; capacity]);
    }
    let mut storage = [0; 64];
    let page = Page::init_with_fences(
        &mut storage,
        Fences {
            lower: Fence::Key(b"a"),
            upper: Fence::Key(&[b'z'; 15]),
        },
    )
    .unwrap();
    assert_eq!(page.free_bytes(), 0);
    let before = page.as_bytes().to_vec();
    assert_eq!(page.try_insert(b"b", b""), Err(PageError::RecordTooLarge));
    assert_eq!(page.as_bytes(), before);
}

#[test]
fn all_point_operations_enforce_half_open_fences_before_size() {
    let mut storage = [0; 64];
    let page = Page::init_with_fences(
        &mut storage,
        Fences {
            lower: Fence::Key(b"a"),
            upper: Fence::Key(b"z"),
        },
    )
    .unwrap();
    assert_eq!(page.free_bytes(), 14);
    assert_eq!(
        &page.as_bytes()[32..48],
        &[63, 0, 1, 0, 0, 0, b'a', 0, 62, 0, 1, 0, 0, 0, b'z', 0]
    );
    page.try_insert(b"a", b"12345").unwrap();
    assert_eq!(page.free_bytes(), 0);
    assert_eq!(page.len(), 1);
    assert_eq!(page.get(b"a"), Ok(Some(live(b"a", b"12345"))));
    assert_eq!(page.get(b"b"), Ok(None));
    for (key, error) in [
        (b"".as_slice(), PageError::EmptyKey),
        (b"\0", PageError::KeyOutOfRange),
        (b"z", PageError::KeyOutOfRange),
        (b"zz", PageError::KeyOutOfRange),
    ] {
        let before = page.as_bytes().to_vec();
        assert_eq!(page.get(key), Err(error));
        assert_eq!(page.try_insert(key, &[0; 64]), Err(error));
        assert_eq!(page.try_insert_record(key, RecordState::Delete), Err(error));
        assert_eq!(
            page.try_upsert(key, RecordState::Cache(&[0; 64])),
            Err(error)
        );
        assert_eq!(page.remove(key), Err(error));
        assert_eq!(page.set_referenced(key, true), Err(error));
        assert_eq!(page.as_bytes(), before);
    }
    let before = page.as_bytes().to_vec();
    assert_eq!(
        page.try_upsert(b"a", RecordState::Insert(&[0; 6])),
        Err(PageError::RecordTooLarge)
    );
    assert_eq!(
        page.try_insert(b"a", &[0; 6]),
        Err(PageError::RecordTooLarge)
    );
    assert_eq!(page.try_insert(b"b", b""), Err(PageError::PageFull));
    assert_eq!(page.as_bytes(), before);
    page.try_upsert(b"a", RecordState::Delete).unwrap();
    page.set_referenced(b"a", true).unwrap();
    let fences: Fences<&[u8]> = Fences {
        lower: Fence::Key(b"a"),
        upper: Fence::Key(b"z"),
    };
    assert_eq!(page.compact(), 0);
    assert_eq!(page.fences(), Some(logical(fences)));
    assert_eq!(page.get(b"a").unwrap().unwrap().state, RecordState::Delete);
    assert!(page.get(b"a").unwrap().unwrap().referenced);
    assert_eq!(page.remove(b"a"), Ok(true));
    assert!(page.is_empty());
    assert_eq!(page.reclaimable_bytes(), 6);
    assert_eq!(page.compact(), 6);
    assert_eq!(page.free_bytes(), 14);
    assert_eq!(page.fences(), Some(logical(fences)));
}

#[test]
fn finite_empty_upper_fence_accepts_no_nonempty_records() {
    let mut storage = [0; 64];
    let page = Page::init_with_fences(
        &mut storage,
        Fences {
            lower: Fence::Unbounded,
            upper: Fence::Key(b""),
        },
    )
    .unwrap();
    assert_eq!(page.get(b"a"), Err(PageError::KeyOutOfRange));
    let mut storage = [0; 64];
    let page = Page::init(&mut storage).unwrap();
    assert_eq!(page.fences(), None);
}

#[test]
fn logical_views_borrow_the_page_rather_than_the_view() {
    let mut storage = [0; 128];
    let page = Page::init_with_fences(
        &mut storage,
        Fences {
            lower: Fence::Key(b"acct/100"),
            upper: Fence::Key(b"acct/200"),
        },
    )
    .unwrap();
    page.try_insert(b"acct/123", b"v").unwrap();
    let page: &Page = page;
    let (lower, upper, record) = {
        // Each view is a temporary; the returned slices borrow only `page`.
        let fences = page.fences().unwrap();
        let (Fence::Key(lower), Fence::Key(upper)) = (fences.lower, fences.upper) else {
            panic!("finite fences");
        };
        let record = page.get(b"acct/123").unwrap().unwrap();
        (lower.suffix(), upper.suffix(), record.key.suffix())
    };
    let bytes = page.as_bytes().as_ptr_range();
    for part in [lower, upper, record] {
        assert!(bytes.contains(&part.as_ptr()));
    }
    // Fences compare with full-key inputs by logical bytes.
    let Some(fences) = page.fences() else {
        panic!("fenced page");
    };
    assert_eq!(
        fences,
        logical(Fences {
            lower: Fence::Key(b"acct/100"),
            upper: Fence::Key(b"acct/200"),
        })
    );
    let Fence::Key(upper) = fences.upper else {
        panic!("finite upper fence");
    };
    assert!(upper.cmp_bytes(b"acct/200").is_eq());
    assert!(upper.cmp_bytes(b"acct/2").is_gt());
    assert_eq!(upper.to_vec(), b"acct/200");
}
