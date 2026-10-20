use super::*;

/// Encode a page with two finite fences and a stored prefix of `prefix_len`
/// bytes, independently of the production encoders.
///
/// The low fence stores its full key. The high fence and each record store
/// the suffix after the prefix, with full logical lengths in their slots and
/// previews of their first two stored bytes. Records must be sorted, in range,
/// and share the prefix. Payloads are packed from the end in slot order.
pub(super) fn compressed_fixture<'s>(
    storage: &'s mut [u8],
    lower: &[u8],
    upper: &[u8],
    prefix_len: usize,
    records: &[(&[u8], RecordState<'_>, bool)],
) -> &'s mut Page {
    assert!(lower < upper && lower[..prefix_len] == upper[..prefix_len]);
    assert!(records.is_sorted_by(|left, right| left.0 < right.0));
    let capacity = storage.len();
    storage.fill(0);
    let count = 2 + records.len();
    let mut end = capacity;
    let mut push = |storage: &mut [u8], index: usize, words: [u16; 3], payload: &[&[u8]]| {
        let preview = payload[0];
        let length: usize = payload.iter().map(|part| part.len()).sum();
        end -= length;
        let mut cursor = end;
        for part in payload {
            storage[cursor..cursor + part.len()].copy_from_slice(part);
            cursor += part.len();
        }
        let slot = 32 + 8 * index;
        storage[slot..slot + 2].copy_from_slice(&(end as u16).to_le_bytes());
        storage[slot + 2..slot + 4].copy_from_slice(&words[1].to_le_bytes());
        storage[slot + 4..slot + 6].copy_from_slice(&words[2].to_le_bytes());
        storage[slot + 6] = preview.first().copied().unwrap_or(0);
        storage[slot + 7] = preview.get(1).copied().unwrap_or(0);
    };
    push(storage, 0, [0, lower.len() as u16, 0], &[lower]);
    push(
        storage,
        1,
        [0, upper.len() as u16, 0],
        &[&upper[prefix_len..]],
    );
    for (index, &(key, state, referenced)) in records.iter().enumerate() {
        assert!(lower <= key && key < upper && !key.is_empty());
        let value = state.value().unwrap_or_default();
        let key_word = ((state.kind() as u16) << 14) | key.len() as u16;
        let value_word = (u16::from(referenced) << 15) | value.len() as u16;
        push(
            storage,
            2 + index,
            [0, key_word, value_word],
            &[&key[prefix_len..], value],
        );
    }
    let gap = end - 32 - 8 * count;
    storage[0..2].copy_from_slice(&(0x8000 | count as u16).to_le_bytes());
    storage[2..4].copy_from_slice(&(capacity as u16).to_le_bytes());
    storage[4..6].copy_from_slice(&(gap as u16).to_le_bytes());
    storage[6..8].copy_from_slice(&(prefix_len as u16).to_le_bytes());
    storage[8..16].fill(0xff);
    Page::mut_from_bytes(storage).unwrap()
}

#[test]
fn worked_example_reads_logical_keys_at_every_stored_prefix_length() {
    let records = [
        (b"acct/123".as_slice(), RecordState::Insert(b"x"), false),
        (b"acct/150", RecordState::Cache(b"y"), true),
    ];
    for prefix_len in 0..=5 {
        let mut storage = [0; 128];
        let page = compressed_fixture(&mut storage, b"acct/100", b"acct/200", prefix_len, &records);
        let page: &Page = page;
        let before = page.as_bytes().to_vec();
        // 32 header + 16 fence slots + 8 + (8 - p) fence keys + 2 * (8 + (8 - p) + 1).
        assert_eq!(page.free_bytes(), 128 - 98 + 3 * prefix_len);
        assert_eq!(page.prefix(), &b"acct/"[..prefix_len]);
        assert_eq!(
            page.fences(),
            Some(logical(Fences {
                lower: Fence::Key(b"acct/100"),
                upper: Fence::Key(b"acct/200"),
            }))
        );
        let Some(Fences {
            lower: Fence::Key(lower),
            upper: Fence::Key(upper),
        }) = page.fences()
        else {
            panic!("finite fences");
        };
        assert_eq!(
            (lower.prefix(), lower.suffix()),
            (b"".as_slice(), b"acct/100".as_slice())
        );
        assert_eq!(upper.suffix(), &b"acct/200"[prefix_len..]);
        // A nonempty prefix borrows the low fence's payload.
        let shares_low_fence =
            |part: &[u8]| part.is_empty() || part.as_ptr() == lower.suffix().as_ptr();
        assert!(shares_low_fence(upper.prefix()));

        for &(key, state, referenced) in &records {
            let record = page.get(key).unwrap().unwrap();
            assert_eq!(
                record,
                RecordRef {
                    key: KeyRef::from(key),
                    state,
                    referenced
                }
            );
            assert_eq!(record.key.prefix(), &key[..prefix_len]);
            assert_eq!(record.key.suffix(), &key[prefix_len..]);
            assert!(shares_low_fence(record.key.prefix()));
            // The value immediately follows the stored suffix.
            let suffix = record.key.suffix().as_ptr_range();
            assert_eq!(record.state.value().unwrap().as_ptr(), suffix.end);
        }
        for key in [
            b"acct/100".as_slice(),
            b"acct/124",
            b"acct/12",
            b"acct/1500",
            b"acct/199\xff",
            b"acct/2",
        ] {
            assert_eq!(page.get(key), Ok(None), "{key:?}");
        }
        for key in [
            b"acct/".as_slice(),
            b"acct",
            b"a",
            b"acct/0",
            b"acct/200",
            b"acct/200\0",
            b"b",
        ] {
            assert_eq!(page.get(key), Err(PageError::KeyOutOfRange), "{key:?}");
        }
        let keys: Vec<_> = page.iter().map(|record| record.key.to_vec()).collect();
        assert_eq!(keys, [b"acct/123", b"acct/150"]);
        assert_eq!(page.as_bytes(), before);
    }
}

#[test]
fn prefix_classified_search_matches_full_key_ordering() {
    let lower = b"pre".as_slice();
    let upper = b"pre\xff\xff".as_slice();
    let keys = [
        b"pre".as_slice(),
        b"pre\0",
        b"pre\0\0",
        b"pre\0\x01",
        b"prea",
        b"prea\0",
        b"preab",
        b"preabc",
        b"preac",
        b"prez",
        b"pre\xff",
        b"pre\xff\x01",
    ];
    let states = [
        RecordState::Insert(b""),
        RecordState::Delete,
        RecordState::Cache(b"cached"),
        RecordState::Phantom,
        RecordState::Insert(b"live"),
    ];
    let records: Vec<_> = keys
        .iter()
        .enumerate()
        .map(|(index, &key)| (key, states[index % states.len()], index % 3 == 0))
        .collect();
    let mut queries: Vec<Vec<u8>> = keys.iter().map(|key| key.to_vec()).collect();
    for query in [
        b"".as_slice(),
        b"\0",
        b"p",
        b"pr",
        b"pra",
        b"prd",
        b"prd\xff",
        b"pre\0\0\0",
        b"prea\0\0",
        b"preb",
        b"pre\xff\x00",
        b"pre\xff\xff",
        b"pre\xff\xff\0",
        b"prf",
        b"q",
        b"\xff",
    ] {
        queries.push(query.to_vec());
    }
    for prefix_len in [0, 1, 3] {
        let mut storage = [0; 512];
        let page = compressed_fixture(&mut storage, lower, upper, prefix_len, &records);
        let page: &Page = page;
        let before = page.as_bytes().to_vec();
        let expected: Vec<_> = records
            .iter()
            .map(|&(key, state, referenced)| RecordRef {
                key: KeyRef::from(key),
                state,
                referenced,
            })
            .collect();
        let actual: Vec<_> = page.iter().collect();
        assert_eq!(actual, expected);
        for query in &queries {
            let below = keys.iter().filter(|key| **key < query.as_slice()).count();
            assert_eq!(page.lower_bound(query), below, "{query:?}");
            let routed = if query.is_empty() {
                Err(PageError::EmptyKey)
            } else if query.as_slice() < lower || query.as_slice() >= upper {
                Err(PageError::KeyOutOfRange)
            } else {
                Ok(expected
                    .iter()
                    .copied()
                    .find(|record| record.key.cmp_bytes(query).is_eq()))
            };
            assert_eq!(page.get(query), routed, "{query:?}");
            for end in &queries {
                if query > end {
                    assert_eq!(
                        page.range(Some(query), Some(end)).err(),
                        Some(PageError::InvalidRange)
                    );
                    continue;
                }
                let inside: Vec<_> = expected
                    .iter()
                    .copied()
                    .filter(|record| {
                        record.key.cmp_bytes(query).is_ge() && record.key.cmp_bytes(end).is_lt()
                    })
                    .collect();
                let actual: Vec<_> = page.range(Some(query), Some(end)).unwrap().collect();
                assert_eq!(actual, inside, "[{query:?}, {end:?})");
            }
        }
        // The record equal to the prefix has an empty stored suffix.
        let first = page.iter().next().unwrap().key;
        assert_eq!(first.suffix(), &b"pre"[prefix_len..]);
        assert_eq!(first.len(), 3);
        assert_eq!(page.as_bytes(), before);
    }
}

#[test]
fn suffix_previews_decide_unequal_comparisons_without_payload_access() {
    let mut storage = [0; 128];
    let records = [
        (b"pre".as_slice(), RecordState::Insert(b""), false),
        (b"preab", RecordState::Insert(b"v"), false),
    ];
    let page = compressed_fixture(&mut storage, b"pre", b"prf", 2, &records);
    assert_eq!(page.prefix(), b"pr");
    // Previews hold the first two suffix bytes: "e\0" and "ea".
    assert_eq!(&page.as_bytes()[54..56], b"e\0");
    assert_eq!(&page.as_bytes()[62..64], b"ea");
    let mut slot = page.slots()[3];
    // An invalid offset in a local slot makes an accidental payload read panic.
    slot.offset.set(u16::MAX);
    assert!(page.compare_suffix(&slot, b"e").is_gt());
    assert!(page.compare_suffix(&slot, b"e\0").is_gt());
    assert!(page.compare_suffix(&slot, b"eb").is_lt());
    assert!(page.compare_suffix(&slot, b"f").is_lt());
}

fn assert_unchanged<T: std::fmt::Debug + PartialEq>(
    page: &mut Page,
    operation: impl FnOnce(&mut Page) -> T,
    expected: T,
) {
    let before = page.as_bytes().to_vec();
    assert_eq!(operation(page), expected);
    assert_eq!(page.as_bytes(), before);
}

#[test]
fn insertions_store_suffixes_matching_the_independent_encoding() {
    let records = [
        (b"acct/123".as_slice(), RecordState::Insert(b"x"), false),
        (b"acct/150", RecordState::Insert(b"y"), false),
    ];
    for prefix_len in 0..=5 {
        let mut expected = [0; 128];
        compressed_fixture(
            &mut expected,
            b"acct/100",
            b"acct/200",
            prefix_len,
            &records,
        );
        let mut storage = [0; 128];
        let page = compressed_fixture(&mut storage, b"acct/100", b"acct/200", prefix_len, &[]);
        for &(key, state, _) in &records {
            page.try_insert_record(key, state).unwrap();
        }
        assert_eq!(page.as_bytes(), expected);
        assert_eq!(page.free_bytes(), 30 + 3 * prefix_len);
        assert_eq!(page.reclaimable_bytes(), 0);
    }
}

#[test]
fn record_size_counts_physical_suffixes_and_compressed_fences() {
    // Empty-page capacity: 128 - 32 header - 16 fence slots - 8 low - 3 high.
    let mut storage = [0; 128];
    let page = compressed_fixture(&mut storage, b"acct/100", b"acct/200", 5, &[]);
    assert_eq!(page.free_bytes(), 69);
    assert_eq!(page.record_capacity(), 69);
    let fits = [0xa5; 69 - 8 - 3];
    assert_unchanged(
        page,
        |page| page.try_insert(b"acct/150", &[0; 59]),
        Err(PageError::RecordTooLarge),
    );
    assert_unchanged(
        page,
        |page| page.try_insert(b"acct/1500", &fits),
        Err(PageError::RecordTooLarge),
    );
    // Without compression, this 8-byte key and 58-byte value would need 74 bytes.
    page.try_insert(b"acct/150", &fits).unwrap();
    assert_eq!(page.free_bytes(), 0);
    assert_eq!(page.get(b"acct/150"), Ok(Some(live(b"acct/150", &fits))));
    for (key, error) in [
        (b"".as_slice(), PageError::EmptyKey),
        (b"acct/", PageError::KeyOutOfRange),
        (b"acct/200", PageError::KeyOutOfRange),
        (b"acct/150", PageError::DuplicateKey),
        (b"acct/151", PageError::PageFull),
    ] {
        assert_unchanged(page, |page| page.try_insert(key, b""), Err(error));
    }
    assert_unchanged(
        page,
        |page| page.try_upsert(b"acct/150", RecordState::Insert(&[1; 59])),
        Err(PageError::RecordTooLarge),
    );
    assert_unchanged(
        page,
        |page| page.try_upsert(b"acct/151", RecordState::Phantom),
        Err(PageError::PageFull),
    );
}

#[test]
fn upsert_transitions_on_compressed_pages_preserve_references_and_holes() {
    let states = |value| {
        [
            RecordState::Insert(value),
            RecordState::Delete,
            RecordState::Cache(value),
            RecordState::Phantom,
        ]
    };
    for old in states(b"old") {
        for value in [b"".as_slice(), b"x", b"new", b"larger"] {
            for new in states(value) {
                for referenced in [false, true] {
                    let mut storage = [0; 256];
                    let page = compressed_fixture(
                        &mut storage,
                        b"acct/100",
                        b"acct/200",
                        5,
                        &[(b"acct/111", RecordState::Insert(b"other"), false)],
                    );
                    assert_eq!(
                        page.try_upsert(b"acct/150", old),
                        Ok(UpsertOutcome::Inserted)
                    );
                    page.set_referenced(b"acct/150", referenced).unwrap();
                    let free = page.free_bytes();
                    let old_len = old.value().map_or(0, <[u8]>::len);
                    let new_len = if new == RecordState::Delete {
                        old_len
                    } else {
                        new.value().map_or(0, <[u8]>::len)
                    };
                    assert_eq!(
                        page.try_upsert(b"acct/150", new),
                        Ok(UpsertOutcome::Replaced)
                    );
                    let expected = RecordRef {
                        key: KeyRef::from(b"acct/150".as_slice()),
                        state: new,
                        referenced,
                    };
                    assert_eq!(page.get(b"acct/150"), Ok(Some(expected)));
                    assert_eq!(page.get(b"acct/111"), Ok(Some(live(b"acct/111", b"other"))));
                    // Growth allocates the 3-byte suffix and the value.
                    let grown = new_len > old_len;
                    assert_eq!(
                        page.free_bytes(),
                        free - if grown { 3 + new_len } else { 0 }
                    );
                    let holes = if grown {
                        3 + old_len
                    } else {
                        old_len - new_len
                    };
                    assert_eq!(page.reclaimable_bytes(), holes);
                    let header = page.as_bytes()[..32].to_vec();
                    assert_eq!(page.compact(), holes);
                    assert_eq!(page.as_bytes()[..4], header[..4]);
                    assert_eq!(page.as_bytes()[6..32], header[6..32]);
                    assert_eq!(page.get(b"acct/150"), Ok(Some(expected)));
                    assert_eq!(page.prefix(), b"acct/");
                }
            }
        }
    }
}

#[test]
fn record_equal_to_the_prefix_uses_a_zero_byte_payload() {
    let mut storage = [0; 64];
    let page = compressed_fixture(&mut storage, b"abc", b"abcd", 3, &[]);
    // 64 - 32 header - 16 fence slots - 3 low fence - 1 high suffix.
    assert_eq!(page.free_bytes(), 12);
    for state in [
        RecordState::Insert(b""),
        RecordState::Delete,
        RecordState::Phantom,
    ] {
        page.try_insert_record(b"abc", state).unwrap();
        assert_eq!(page.free_bytes(), 4);
        let slot = &page.as_bytes()[48..56];
        let offset = usize::from(read_u16(slot, 0));
        assert_eq!(offset, 32 + 3 * 8 + 4);
        assert_eq!(read_u16(slot, 2) & 0x3fff, 3);
        assert_eq!(read_u16(slot, 4), 0);
        assert_eq!(&slot[6..8], &[0, 0]);
        let record = page.get(b"abc").unwrap().unwrap();
        assert_eq!(record.key.to_vec(), b"abc");
        assert!(record.key.suffix().is_empty() && !record.key.is_empty());
        assert_eq!(record.state, state);
        assert_eq!(page.reclaimable_bytes(), 0);
        assert_eq!(page.compact(), 0);
        for (key, error) in [
            (b"abcd".as_slice(), PageError::KeyOutOfRange),
            (b"abc", PageError::DuplicateKey),
            (b"abc\0", PageError::PageFull),
        ] {
            assert_unchanged(page, |page| page.try_insert(key, b""), Err(error));
        }
        assert_eq!(page.remove(b"abc"), Ok(true));
        assert_eq!((page.free_bytes(), page.reclaimable_bytes()), (12, 0));
    }
}

#[test]
fn removing_every_record_preserves_fences_and_the_stored_prefix() {
    let mut storage = [0; 256];
    let page = compressed_fixture(&mut storage, b"key/a", b"key/z", 4, &[]);
    page.header
        .count_and_flags
        .set(page.header.count_and_flags.get() | 0x2000);
    page.as_mut_bytes()[16..32].fill(0xa5);
    let metadata = page.as_bytes()[6..32].to_vec();
    let empty_free = page.free_bytes();
    let keys = [b"key/b".as_slice(), b"key/bb", b"key/c", b"key/y\xff"];
    for (index, &key) in keys.iter().enumerate() {
        page.try_insert(key, &vec![index as u8; index]).unwrap();
        page.set_referenced(key, true).unwrap();
    }
    page.try_upsert(b"key/c", RecordState::Delete).unwrap();
    let mut holes = 0;
    for (index, &key) in keys.iter().enumerate().rev() {
        assert_eq!(page.remove(key), Ok(true));
        holes += key.len() - 4 + index;
        assert_eq!(page.reclaimable_bytes(), holes);
        assert!(page.iter().all(|record| record.referenced));
    }
    assert!(page.is_empty());
    assert_eq!(page.compact(), holes);
    assert_eq!(page.free_bytes(), empty_free);
    assert_eq!(&page.as_bytes()[6..32], metadata);
    assert_eq!(read_u16(page.as_bytes(), 0), 0xa002);
    assert_eq!(page.prefix(), b"key/");
    assert_eq!(
        page.fences(),
        Some(logical(Fences {
            lower: Fence::Key(b"key/a"),
            upper: Fence::Key(b"key/z"),
        }))
    );
    page.try_insert(b"key/m", b"again").unwrap();
    assert_eq!(page.get(b"key/m"), Ok(Some(live(b"key/m", b"again"))));
}

#[test]
fn stored_zero_prefix_with_compressible_bounds_mutates_full_keys() {
    let mut storage = [0; 128];
    let page = compressed_fixture(&mut storage, b"acct/100", b"acct/200", 0, &[]);
    page.try_insert(b"acct/150", b"v").unwrap();
    let slot = &page.as_bytes()[48..56];
    let offset = usize::from(read_u16(slot, 0));
    assert_eq!(&slot[6..8], b"ac");
    assert_eq!(&page.as_bytes()[offset..offset + 9], b"acct/150v");
    page.try_upsert(b"acct/150", RecordState::Insert(b"vv"))
        .unwrap();
    assert_eq!(page.reclaimable_bytes(), 9);
    assert_eq!(page.compact(), 9);
    assert_eq!(read_u16(page.as_bytes(), 6), 0);
    assert_eq!(
        page.get(b"acct/150").unwrap().unwrap().key.suffix(),
        b"acct/150"
    );
}
