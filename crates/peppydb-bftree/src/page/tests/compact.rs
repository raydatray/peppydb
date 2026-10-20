use super::*;

#[test]
fn removal_releases_slots_and_compaction_recovers_payloads() {
    for order in [[0, 1, 2, 3], [3, 2, 1, 0], [1, 3, 0, 2]] {
        let mut storage = [0; 128];
        let page = Page::init(&mut storage).unwrap();
        let states = [
            RecordState::Insert(b"123"),
            RecordState::Delete,
            RecordState::Cache(b"456"),
            RecordState::Phantom,
        ];
        for (index, state) in states.into_iter().enumerate() {
            page.try_insert_record(&[b'a' + index as u8], state)
                .unwrap();
            page.set_referenced(&[b'a' + index as u8], true).unwrap();
        }
        let before = page.as_bytes().to_vec();
        assert_eq!(page.remove(b""), Err(PageError::EmptyKey));
        assert_eq!(page.remove(b"missing"), Ok(false));
        assert_eq!(page.as_bytes(), before);
        let mut holes = 0;
        for index in order {
            let free = page.free_bytes();
            let key = [b'a' + index as u8];
            assert_eq!(page.remove(&key), Ok(true));
            assert_eq!(page.free_bytes(), free + 8);
            holes += 1 + states[index].value().map_or(0, <[u8]>::len);
            assert_eq!(page.reclaimable_bytes(), holes);
            let before = page.as_bytes().to_vec();
            assert_eq!(page.remove(&key), Ok(false));
            assert_eq!(page.as_bytes(), before);
            assert_eq!(page.get(&key), Ok(None));
            assert!(page.iter().all(|record| record.referenced));
        }
        assert!(page.is_empty());
        assert_eq!(page.compact(), holes);
        assert_eq!(page.free_bytes(), 96);
        assert_eq!(page.reclaimable_bytes(), 0);
        assert_eq!(page.compact(), 0);
    }
}

#[test]
fn fragmented_insertion_fails_without_implicit_compaction() {
    let mut storage = [0; 64];
    let page = Page::init(&mut storage).unwrap();
    page.try_insert(b"a", &[7; 23]).unwrap();
    page.remove(b"a").unwrap();
    assert_eq!(page.free_bytes(), 8);
    let before = page.as_bytes().to_vec();
    assert_eq!(page.try_insert(b"b", b""), Err(PageError::PageFull));
    assert_eq!(page.as_bytes(), before);
    assert_eq!(page.compact(), 24);
    page.try_insert(b"b", &[9; 23]).unwrap();
    assert_eq!(page.free_bytes(), 0);
}

#[test]
fn compaction_preserves_physical_tombstone_values_and_metadata() {
    let mut storage = [0; 128];
    let page = Page::init(&mut storage).unwrap();
    for (key, state) in [
        (b"a", RecordState::Insert(b"live")),
        (b"b", RecordState::Insert(b"retained")),
        (b"c", RecordState::Cache(b"cached")),
        (b"d", RecordState::Phantom),
        (b"e", RecordState::Delete),
    ] {
        page.try_insert_record(key, state).unwrap();
        page.set_referenced(key, true).unwrap();
    }
    // Make a metadata-only tombstone before upsert is implemented.
    page.as_mut_bytes()[43] |= 0x40;
    page.header
        .count_and_flags
        .set(page.header.count_and_flags.get() | 0x3000);
    page.as_mut_bytes()[8..32].fill(0xa5);
    page.remove(b"a").unwrap();
    let metadata = page.as_bytes()[6..32].to_vec();
    let records: Vec<_> = page
        .iter()
        .map(|record| {
            (
                record.key.to_vec(),
                record.state.kind(),
                record.state.value().map(<[u8]>::to_vec),
                record.referenced,
            )
        })
        .collect();
    let free = page.free_bytes();
    assert_eq!(page.reclaimable_bytes(), 5);
    assert_eq!(page.compact(), 5);
    assert_eq!(page.free_bytes(), free + 5);
    assert_eq!(page.reclaimable_bytes(), 0);
    assert_eq!(&page.as_bytes()[6..32], metadata);
    assert_eq!(read_u16(page.as_bytes(), 0), 0x3004);
    let actual: Vec<_> = page
        .iter()
        .map(|record| {
            (
                record.key.to_vec(),
                record.state.kind(),
                record.state.value().map(<[u8]>::to_vec),
                record.referenced,
            )
        })
        .collect();
    assert_eq!(actual, records);
    let offset = usize::from(read_u16(page.as_bytes(), 32));
    assert_eq!(read_u16(page.as_bytes(), 36), 0x8008);
    assert_eq!(&page.as_bytes()[offset..offset + 9], b"bretained");
    assert_eq!(page.get(b"b").unwrap().unwrap().state, RecordState::Delete);
    assert_eq!(page.compact(), 0);
}
