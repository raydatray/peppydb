use super::*;

fn assert_rejected(page: &mut Page, key: &[u8], value: &[u8], error: PageError) {
    let before = page.as_bytes().to_vec();
    assert_eq!(page.try_insert(key, value), Err(error));
    assert_eq!(page.as_bytes(), before);
}

#[test]
fn insert_worked_example_matches_golden_bytes() {
    let mut storage = [0; 128];
    let page = Page::init(&mut storage).unwrap();
    page.try_insert(b"30", b"cat").unwrap();
    let first_payload = page.as_bytes()[123..].to_vec();
    page.try_insert(b"10", b"dog").unwrap();
    assert_eq!(&page.as_bytes()[123..], first_payload);
    assert_eq!(page.free_bytes(), 70);
    let old_payloads = page.as_bytes()[118..].to_vec();
    page.try_insert(b"20", b"owl").unwrap();
    assert_eq!(&page.as_bytes()[118..], old_payloads);
    assert_eq!(page.len(), 3);
    assert_eq!(page.free_bytes(), 57);
    assert_eq!(32 + page.len() * 8 + page.free_bytes(), 113);
    assert_eq!(
        &page.as_bytes()[32..56],
        &[
            118, 0, 2, 0, 3, 0, b'1', b'0', 113, 0, 2, 0, 3, 0, b'2', b'0', 123, 0, 2, 0, 3, 0,
            b'3', b'0',
        ]
    );
    assert_eq!(&page.as_bytes()[113..], b"20owl10dog30cat");
    for (key, value) in [(b"10", b"dog"), (b"20", b"owl"), (b"30", b"cat")] {
        assert_eq!(page.get(key), Ok(Some(live(key, value))));
    }
    assert_eq!(page.get(b"25"), Ok(None));
}

#[test]
fn insert_prefixes_zero_bytes_and_empty_values() {
    let keys = [b"abc".as_slice(), b"\0\0", b"ab", b"\0", b"\xff", b"a"];
    let sorted = [b"\0".as_slice(), b"\0\0", b"a", b"ab", b"abc", b"\xff"];
    let mut storage = [0; 128];
    let page = Page::init(&mut storage).unwrap();
    for key in keys {
        page.try_insert(key, b"").unwrap();
        assert_eq!(page.get(key), Ok(Some(live(key, b""))));
    }
    for (index, key) in sorted.into_iter().enumerate() {
        assert_eq!(page.record_at(index), live(key, b""));
        let slot = 32 + index * 8;
        assert_eq!(read_u16(page.as_bytes(), slot + 2), key.len() as u16);
        assert_eq!(read_u16(page.as_bytes(), slot + 4), 0);
        assert_eq!(
            &page.as_bytes()[slot + 6..slot + 8],
            &[key[0], key.get(1).copied().unwrap_or(0)]
        );
    }
    assert_rejected(page, b"ab", b"new", PageError::DuplicateKey);
}

#[test]
fn exact_fits_and_error_precedence_preserve_bytes() {
    let mut storage = [0; 64];
    let page = Page::init(&mut storage).unwrap();
    // Exact fit: 32 header bytes + 8 slot bytes + 1 key byte + 23 value bytes.
    let value = vec![0xa5; 23];
    let too_large = vec![0xa5; value.len() + 1];
    assert_rejected(page, b"", &too_large, PageError::EmptyKey);
    assert_rejected(page, b"k", &too_large, PageError::RecordTooLarge);
    assert_rejected(page, &vec![0; 0x4000], b"", PageError::RecordTooLarge);
    assert_rejected(page, b"k", &vec![0; 0x8000], PageError::RecordTooLarge);
    page.try_insert(b"k", &value).unwrap();
    assert_eq!(page.free_bytes(), 0);
    assert_eq!(page.len(), 1);
    assert_eq!(page.get(b"k"), Ok(Some(live(b"k", &value))));
    assert_rejected(page, b"", &too_large, PageError::EmptyKey);
    assert_rejected(page, b"k", &too_large, PageError::RecordTooLarge);
    assert_rejected(page, b"k", b"", PageError::DuplicateKey);
    assert_rejected(page, b"x", b"", PageError::PageFull);
}

#[test]
fn insertion_preserves_flags_and_inactive_metadata() {
    let mut storage = [0; 128];
    let page = Page::init(&mut storage).unwrap();
    page.as_mut_bytes()[0..2].copy_from_slice(&0xb000u16.to_le_bytes());
    page.as_mut_bytes()[6..32].fill(0xa5);
    let inactive = page.as_bytes()[6..32].to_vec();
    for (index, key) in [b"30", b"10", b"20"].into_iter().enumerate() {
        page.try_insert(key, b"value").unwrap();
        assert_eq!(page.len(), index + 1);
        assert_eq!(read_u16(page.as_bytes(), 0), 0xb000 | (index + 1) as u16);
        assert_eq!(&page.as_bytes()[6..32], inactive);
        assert_eq!(page.capacity(), 128);
    }
}

#[test]
fn typed_insertions_and_reference_updates_preserve_other_bytes() {
    let states = [
        RecordState::Insert(b""),
        RecordState::Delete,
        RecordState::Cache(b"value"),
        RecordState::Phantom,
    ];
    for order in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1]] {
        let mut storage = [0; 128];
        let page = Page::init(&mut storage).unwrap();
        for index in order {
            let key = [b'a' + index as u8];
            page.try_insert_record(&key, states[index]).unwrap();
        }
        for (index, state) in states.into_iter().enumerate() {
            let key = [b'a' + index as u8];
            assert_eq!(page.get(&key).unwrap().unwrap().state, state);
            let slot = 32 + index * 8;
            assert_eq!(
                read_u16(page.as_bytes(), slot + 2),
                ((state.kind() as u16) << 14) | 1
            );
            assert_eq!(
                read_u16(page.as_bytes(), slot + 4) as usize,
                state.value().map_or(0, <[u8]>::len)
            );
            for incoming in states {
                let before = page.as_bytes().to_vec();
                assert_eq!(
                    page.try_insert_record(&key, incoming),
                    Err(PageError::DuplicateKey)
                );
                assert_eq!(page.as_bytes(), before);
            }
            for referenced in [true, true, false] {
                let mut expected = page.as_bytes().to_vec();
                expected[slot + 5] = (expected[slot + 5] & 0x7f) | (u8::from(referenced) << 7);
                assert_eq!(page.set_referenced(&key, referenced), Ok(true));
                assert_eq!(page.as_bytes(), expected);
                assert_eq!(page.get(&key).unwrap().unwrap().referenced, referenced);
            }
        }
        let before = page.as_bytes().to_vec();
        assert_eq!(page.set_referenced(b"missing", true), Ok(false));
        assert_eq!(page.set_referenced(b"", true), Err(PageError::EmptyKey));
        assert_eq!(
            page.try_insert_record(b"", RecordState::Delete),
            Err(PageError::EmptyKey)
        );
        assert_eq!(page.as_bytes(), before);
    }
}

#[test]
fn every_state_can_exactly_fill_a_page() {
    for state in [
        RecordState::Insert(b""),
        RecordState::Delete,
        RecordState::Cache(b""),
        RecordState::Phantom,
    ] {
        let mut storage = [0; 64];
        let page = Page::init(&mut storage).unwrap();
        let key = [b'k'; 24];
        page.try_insert_record(&key, state).unwrap();
        assert_eq!(page.free_bytes(), 0);
        let before = page.as_bytes().to_vec();
        assert_eq!(
            page.try_insert_record(&[b'k'; 25], state),
            Err(PageError::RecordTooLarge)
        );
        assert_eq!(
            page.try_insert_record(&key, state),
            Err(PageError::DuplicateKey)
        );
        assert_eq!(
            page.try_insert_record(b"x", state),
            Err(PageError::PageFull)
        );
        assert_eq!(page.as_bytes(), before);
    }
}

#[test]
fn upsert_all_state_transitions_and_sizes_preserve_references() {
    for old in [
        RecordState::Insert(b"old"),
        RecordState::Delete,
        RecordState::Cache(b"old"),
        RecordState::Phantom,
    ] {
        for value in [b"".as_slice(), b"x", b"new", b"larger"] {
            for new in [
                RecordState::Insert(value),
                RecordState::Delete,
                RecordState::Cache(value),
                RecordState::Phantom,
            ] {
                for referenced in [false, true] {
                    let mut storage = [0; 128];
                    let page = Page::init(&mut storage).unwrap();
                    page.try_insert(b"a", b"other").unwrap();
                    assert_eq!(page.try_upsert(b"k", old), Ok(UpsertOutcome::Inserted));
                    page.set_referenced(b"k", referenced).unwrap();
                    let free = page.free_bytes();
                    let old_len = old.value().map_or(0, <[u8]>::len);
                    let new_len = if new == RecordState::Delete {
                        old_len
                    } else {
                        new.value().map_or(0, <[u8]>::len)
                    };
                    assert_eq!(page.try_upsert(b"k", new), Ok(UpsertOutcome::Replaced));
                    assert_eq!(page.len(), 2);
                    assert_eq!(
                        page.get(b"k"),
                        Ok(Some(RecordRef {
                            key: b"k",
                            state: new,
                            referenced
                        }))
                    );
                    assert_eq!(page.get(b"a"), Ok(Some(live(b"a", b"other"))));
                    assert_eq!(usize::from(read_u16(page.as_bytes(), 44) & 0x7fff), new_len);
                    assert_eq!(
                        page.free_bytes(),
                        free - if new_len > old_len { 1 + new_len } else { 0 }
                    );
                    assert_eq!(
                        page.reclaimable_bytes(),
                        if new_len > old_len {
                            1 + old_len
                        } else {
                            old_len - new_len
                        }
                    );
                    let holes = page.reclaimable_bytes();
                    assert_eq!(page.compact(), holes);
                    assert_eq!(page.get(b"k").unwrap().unwrap().state, new);
                }
            }
        }
    }
}

#[test]
fn deletion_on_full_page_is_metadata_only_and_resurrects_in_place() {
    let mut storage = [0; 64];
    let page = Page::init(&mut storage).unwrap();
    page.try_insert(b"k", &[7; 23]).unwrap();
    page.set_referenced(b"k", true).unwrap();
    let mut expected = page.as_bytes().to_vec();
    expected[35] |= 0x40;
    assert_eq!(
        page.try_upsert(b"k", RecordState::Delete),
        Ok(UpsertOutcome::Replaced)
    );
    assert_eq!(page.as_bytes(), expected);
    assert_eq!(page.free_bytes(), 0);
    assert_eq!(page.reclaimable_bytes(), 0);
    assert_eq!(page.compact(), 0);
    assert_eq!(read_u16(page.as_bytes(), 36), 0x8017);
    page.try_upsert(b"k", RecordState::Cache(&[9; 23])).unwrap();
    assert_eq!(
        page.get(b"k").unwrap().unwrap().state,
        RecordState::Cache(&[9; 23])
    );
    assert!(page.get(b"k").unwrap().unwrap().referenced);
    page.try_upsert(b"k", RecordState::Phantom).unwrap();
    assert_eq!(read_u16(page.as_bytes(), 36), 0x8000);
    assert_eq!(page.reclaimable_bytes(), 23);
}

#[test]
fn upsert_capacity_checks_do_not_credit_the_old_allocation() {
    let mut storage = [0; 64];
    let page = Page::init(&mut storage).unwrap();
    page.try_upsert(b"k", RecordState::Insert(&[7; 23]))
        .unwrap();
    page.try_upsert(b"k", RecordState::Insert(b"abc")).unwrap();
    assert_eq!(page.free_bytes(), 0);
    assert_eq!(page.reclaimable_bytes(), 20);
    for (key, value, error) in [
        (b"".as_slice(), 24, PageError::EmptyKey),
        (b"k", 24, PageError::RecordTooLarge),
        (b"k", 22, PageError::PageFull),
        (b"x", 0, PageError::PageFull),
    ] {
        let before = page.as_bytes().to_vec();
        assert_eq!(
            page.try_upsert(key, RecordState::Insert(&vec![9; value])),
            Err(error)
        );
        assert_eq!(page.as_bytes(), before);
    }
    assert_eq!(page.compact(), 20);
    let before = page.as_bytes().to_vec();
    assert_eq!(
        page.try_upsert(b"k", RecordState::Insert(&[9; 22])),
        Err(PageError::PageFull)
    );
    assert_eq!(page.as_bytes(), before);
    assert_eq!(
        page.try_upsert(b"k", RecordState::Insert(&[9; 19])),
        Ok(UpsertOutcome::Replaced)
    );
    assert_eq!(page.free_bytes(), 0);
    assert_eq!(page.reclaimable_bytes(), 4);
}
