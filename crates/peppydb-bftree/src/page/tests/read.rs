use super::*;

#[test]
fn lookup_worked_byte_fixture() {
    let mut storage = [0; 128];
    let page = Page::init(&mut storage).unwrap();
    page.as_mut_bytes()[0..2].copy_from_slice(&[3, 0x20]);
    // Free space: 113 - 32 header bytes - 3 slots * 8 bytes.
    page.as_mut_bytes()[4..6].copy_from_slice(&57u16.to_le_bytes());
    page.as_mut_bytes()[32..56].copy_from_slice(&[
        118, 0, 2, 0, 3, 0, b'1', b'0', // 10/dog
        113, 0, 2, 0, 3, 0, b'2', b'0', // 20/owl
        123, 0, 2, 0, 3, 0, b'3', b'0', // 30/cat
    ]);
    page.as_mut_bytes()[113..128].copy_from_slice(b"20owl10dog30cat");
    let before = page.as_bytes().to_vec();
    assert_eq!(page.len(), 3);
    assert!(!page.is_empty());
    assert_eq!(page.free_bytes(), 57);
    for (key, value) in [(b"10", b"dog"), (b"20", b"owl"), (b"30", b"cat")] {
        assert_eq!(page.get(key), Ok(Some(live(key, value))));
    }
    assert_eq!(
        page.get(b"20")
            .unwrap()
            .unwrap()
            .state
            .value()
            .unwrap()
            .as_ptr(),
        page.as_bytes()[115..].as_ptr()
    );
    for key in [b"01".as_slice(), b"15", b"25", b"40", b"20\0"] {
        assert_eq!(page.get(key), Ok(None));
    }
    assert_eq!(page.get(b""), Err(PageError::EmptyKey));
    assert_eq!(page.as_bytes(), before);
}

#[test]
fn lookup_prefixes_preview_collisions_and_empty_values() {
    let mut storage = [0; 128];
    let page = Page::init(&mut storage).unwrap();
    page.as_mut_bytes()[0..2].copy_from_slice(&[4, 0]);
    // Free space: 118 - 32 header bytes - 4 slots * 8 bytes.
    page.as_mut_bytes()[4..6].copy_from_slice(&54u16.to_le_bytes());
    page.as_mut_bytes()[32..64].copy_from_slice(&[
        118, 0, 1, 0, 1, 0, 0, 0, // \0/A
        120, 0, 2, 0, 0, 0, 0, 0, // \0\0/empty
        122, 0, 2, 0, 0, 0, b'a', b'b', // ab/empty
        124, 0, 3, 0, 1, 0, b'a', b'b', // abc/Z
    ]);
    page.as_mut_bytes()[118..128].copy_from_slice(b"\0A\0\0ababcZ");
    for (key, value) in [
        (b"\0".as_slice(), b"A".as_slice()),
        (b"\0\0", b""),
        (b"ab", b""),
        (b"abc", b"Z"),
    ] {
        assert_eq!(page.get(key), Ok(Some(live(key, value))));
    }
    for key in [b"\0\0\0".as_slice(), b"a", b"abb", b"abcd", b"\xff"] {
        assert_eq!(page.get(key), Ok(None));
    }
}

#[test]
fn slot_lengths_exclude_all_kind_and_reference_bits() {
    for kind in [
        RecordKind::Insert,
        RecordKind::Delete,
        RecordKind::Cache,
        RecordKind::Phantom,
    ] {
        for referenced in [false, true] {
            let mut storage = [0; 128];
            let page = Page::init(&mut storage).unwrap();
            page.as_mut_bytes()[0..2].copy_from_slice(&[1, 0]);
            // Free space: 126 - 32 header bytes - 1 slot * 8 bytes.
            page.as_mut_bytes()[4..6].copy_from_slice(&86u16.to_le_bytes());
            let key_word = ((kind as u16) << 14) | 1;
            let value_word = (u16::from(referenced) << 15) | 1;
            page.as_mut_bytes()[32..40].copy_from_slice(&[
                126,
                0,
                key_word as u8,
                (key_word >> 8) as u8,
                value_word as u8,
                (value_word >> 8) as u8,
                b'k',
                0,
            ]);
            page.as_mut_bytes()[126..128].copy_from_slice(b"kV");
            let before = page.as_bytes().to_vec();
            // Check physical lengths independently of logical absence.
            assert_eq!(page.slots()[0].key_range(), 126..127);
            assert_eq!(page.slots()[0].value_len(), 1);
            let state = match kind {
                RecordKind::Insert => RecordState::Insert(b"V"),
                RecordKind::Delete => RecordState::Delete,
                RecordKind::Cache => RecordState::Cache(b"V"),
                RecordKind::Phantom => RecordState::Phantom,
            };
            let expected = RecordRef {
                key: b"k",
                state,
                referenced,
            };
            assert_eq!(state.kind(), kind);
            assert_eq!(page.get(b"k"), Ok(Some(expected)));
            let mut iter = page.iter();
            assert_eq!(iter.len(), 1);
            assert_eq!(iter.next(), Some(expected));
            assert_eq!(iter.len(), 0);
            assert_eq!(iter.next(), None);
            assert_eq!(iter.next(), None);
            assert_eq!(page.as_bytes(), before);
        }
    }
}

#[test]
fn lookup_empty_page_and_empty_key() {
    let mut storage = [0; 64];
    let page = Page::init(&mut storage).unwrap();
    let before = page.as_bytes().to_vec();
    assert_eq!(page.get(b"missing"), Ok(None));
    assert_eq!(page.get(b""), Err(PageError::EmptyKey));
    assert_eq!(page.as_bytes(), before);
}

#[test]
fn iterate_empty_page() {
    let mut storage = [0; 64];
    let page: &Page = Page::init(&mut storage).unwrap();
    let mut iter = page.iter();
    assert_eq!(iter.len(), 0);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next(), None);
    assert_eq!(IntoIterator::into_iter(page).count(), 0);
}

#[test]
fn iterate_borrowed_records_in_key_order() {
    let records = [
        (b"\0".as_slice(), b"".as_slice()),
        (b"\0\0", b"zero"),
        (b"ab", b"short"),
        (b"abc", b""),
        (b"abcd", b"long"),
        (b"\xff", b"\0"),
    ];
    for order in [[0, 1, 2, 3, 4, 5], [5, 4, 3, 2, 1, 0], [2, 5, 1, 4, 0, 3]] {
        let mut storage = [0; 128];
        let page = Page::init(&mut storage).unwrap();
        for index in order {
            let (key, value) = records[index];
            page.try_insert(key, value).unwrap();
        }
        let page: &Page = page;
        let before = page.as_bytes().to_vec();
        let mut iter = page.iter();
        for (index, &(key, value)) in records.iter().enumerate() {
            assert_eq!(iter.len(), records.len() - index);
            let actual = iter.next().unwrap();
            assert_eq!(actual, live(key, value));
            let offset = usize::from(read_u16(page.as_bytes(), 32 + index * 8));
            assert_eq!(actual.key.as_ptr(), page.as_bytes()[offset..].as_ptr());
            assert_eq!(
                actual.state.value().unwrap().as_ptr(),
                page.get(key)
                    .unwrap()
                    .unwrap()
                    .state
                    .value()
                    .unwrap()
                    .as_ptr()
            );
        }
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next(), None);
        let mut expected = records.iter().map(|&(key, value)| live(key, value));
        for record in page {
            assert_eq!(Some(record), expected.next());
        }
        assert_eq!(expected.next(), None);
        assert_eq!(page.as_bytes(), before);
    }
}

#[test]
fn bounded_iteration_matches_ordered_filtering_with_and_without_fences() {
    let keys = [
        b"\0".as_slice(),
        b"\0\0",
        b"a",
        b"ab",
        b"abc",
        b"abd",
        b"b",
        b"\xff",
    ];
    let bounds = [
        None,
        Some(b"".as_slice()),
        Some(b"\0"),
        Some(b"\0\0"),
        Some(b"aa"),
        Some(b"ab"),
        Some(b"abc"),
        Some(b"abcd"),
        Some(b"z"),
        Some(b"\xff"),
        Some(b"\xff\xff"),
    ];
    for fenced in [false, true] {
        let mut storage = [0; 256];
        let page = if fenced {
            Page::init_with_fences(
                &mut storage,
                Fences {
                    lower: Fence::Key(b"a"),
                    upper: Fence::Key(b"z"),
                },
            )
            .unwrap()
        } else {
            Page::init(&mut storage).unwrap()
        };
        let mut model = BTreeMap::new();
        for (index, key) in keys.into_iter().enumerate() {
            if fenced && !(b"a".as_slice()..b"z").contains(&key) {
                continue;
            }
            let state = [
                RecordState::Insert(b""),
                RecordState::Delete,
                RecordState::Cache(b"value"),
                RecordState::Phantom,
            ][index % 4];
            page.try_insert_record(key, state).unwrap();
            page.set_referenced(key, index % 2 == 0).unwrap();
            model.insert(
                key,
                RecordRef {
                    key,
                    state,
                    referenced: index % 2 == 0,
                },
            );
        }
        let before = page.as_bytes().to_vec();
        for start in bounds {
            for end in bounds {
                if start.zip(end).is_some_and(|(start, end)| start > end) {
                    assert_eq!(page.range(start, end).err(), Some(PageError::InvalidRange));
                    continue;
                }
                let expected: Vec<_> = model
                    .values()
                    .copied()
                    .filter(|record| {
                        start.is_none_or(|start| record.key >= start)
                            && end.is_none_or(|end| record.key < end)
                    })
                    .collect();
                let mut actual = page.range(start, end).unwrap();
                assert_eq!(actual.len(), expected.len());
                for (index, record) in expected.iter().enumerate() {
                    assert_eq!(
                        actual.size_hint(),
                        (expected.len() - index, Some(expected.len() - index))
                    );
                    assert_eq!(actual.next(), Some(*record));
                }
                assert_eq!(actual.len(), 0);
                assert_eq!(actual.next(), None);
                assert_eq!(actual.next(), None);
            }
        }
        let unbounded: Vec<_> = page.range(None, None).unwrap().collect();
        let all: Vec<_> = page.iter().collect();
        assert_eq!(unbounded, all);
        assert_eq!(page.as_bytes(), before);
        // Output lifetime is independent of the query's allocated storage.
        let records = {
            let start = b"a".to_vec();
            let end = b"z".to_vec();
            page.range(Some(&start), Some(&end)).unwrap()
        };
        assert_eq!(records.count(), 5);
    }
}

#[test]
fn ranges_on_empty_pages_obey_bound_ordering_only() {
    let mut storage = [0; 64];
    let page = Page::init_with_fences(
        &mut storage,
        Fences {
            lower: Fence::Unbounded,
            upper: Fence::Key(b""),
        },
    )
    .unwrap();
    for (start, end) in [
        (None, None),
        (Some(b"".as_slice()), None),
        (None, Some(b"".as_slice())),
        (Some(b"a".as_slice()), Some(b"a".as_slice())),
        (Some(b"a".as_slice()), Some(b"z".as_slice())),
    ] {
        assert_eq!(page.range(start, end).unwrap().len(), 0);
    }
    assert_eq!(
        page.range(Some(b"z"), Some(b"a")).err(),
        Some(PageError::InvalidRange)
    );
}
