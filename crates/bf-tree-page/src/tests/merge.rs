use super::*;

fn dead(key: &[u8]) -> RecordRef<'_> {
    RecordRef {
        key: KeyRef::from(key),
        state: RecordState::Delete,
    }
}

fn contents(page: &Page) -> Vec<(Vec<u8>, Vec<u8>)> {
    page.iter()
        .map(|record| {
            assert_eq!(
                record.state.kind(),
                RecordKind::Insert,
                "outputs hold only Insert"
            );
            (record.key.to_vec(), record.state.value().unwrap().to_vec())
        })
        .collect()
}

fn base(capacity: usize, lower: &[u8], upper: &[u8], keys: &[&[u8]]) -> Vec<u8> {
    let mut bytes = vec![0; capacity];
    let fences = Fences {
        lower: Fence::Key(lower),
        upper: Fence::Key(upper),
    };
    let page = Page::init_with_fences(&mut bytes, fences).unwrap();
    for key in keys {
        page.try_insert(key, b"base").unwrap();
    }
    bytes
}

#[test]
fn deletes_remove_base_keys_and_ignore_missing_ones() {
    let mut bytes = base(256, b"a", b"z", &[b"b", b"d", b"f"]);
    let page = Page::mut_from_bytes(&mut bytes).unwrap();
    let batch = [
        live(b"c", b"new"),
        dead(b"d"),
        dead(b"e"),
        live(b"f", b"new"),
    ];

    assert_eq!(page.merge_batch_fits(batch.iter().copied()), Ok(true));
    page.merge_batch_in_place(batch.iter().copied()).unwrap();
    assert_eq!(
        contents(page),
        [
            (b"b".to_vec(), b"base".to_vec()),
            (b"c".to_vec(), b"new".to_vec()),
            (b"f".to_vec(), b"new".to_vec()),
        ]
    );
    Page::validate(&bytes).unwrap();
}

#[test]
fn an_empty_batch_changes_nothing() {
    let mut bytes = base(256, b"a", b"z", &[b"b", b"d"]);
    let page = Page::mut_from_bytes(&mut bytes).unwrap();
    // Leave a hole, which a rebuild would pack.
    page.try_upsert(b"b", RecordState::Insert(b"x")).unwrap();
    let before = bytes.clone();
    let page = Page::mut_from_bytes(&mut bytes).unwrap();
    page.merge_batch_in_place([].into_iter()).unwrap();
    assert_eq!(bytes, before);
}

#[test]
fn invalid_batches_are_rejected_with_both_buffers_unchanged() {
    let mut bytes = base(256, b"b", b"y", &[b"c"]);
    let before = bytes.clone();
    let cases: [(&[RecordRef<'_>], PageError); 5] = [
        (
            &[live(b"d", b"1"), live(b"c", b"1")],
            PageError::InvalidBatch("batch keys are not strictly increasing"),
        ),
        (
            &[live(b"d", b"1"), dead(b"d")],
            PageError::InvalidBatch("batch keys are not strictly increasing"),
        ),
        (&[live(b"a", b"1")], PageError::KeyOutOfRange),
        (&[live(b"", b"1")], PageError::EmptyKey),
        (
            &[RecordRef {
                key: KeyRef::from(b"d".as_slice()),
                state: RecordState::Cache(b"1"),
            }],
            PageError::InvalidBatch("batch holds a clean record"),
        ),
    ];
    for (batch, error) in cases {
        let page = Page::mut_from_bytes(&mut bytes).unwrap();
        assert_eq!(page.merge_batch_fits(batch.iter().copied()), Err(error));
        assert_eq!(page.merge_batch_in_place(batch.iter().copied()), Err(error));
        let mut right = [0xa5; 256];
        assert_eq!(
            page.merge_batch_split(&mut right, batch.iter().copied())
                .unwrap_err(),
            error
        );
        assert_eq!(bytes, before);
        assert_eq!(right, [0xa5; 256]);
    }

    let mut unfenced = vec![0; 256];
    let page = Page::init(&mut unfenced).unwrap();
    assert_eq!(
        page.merge_batch_fits([live(b"a", b"1")].into_iter()),
        Err(PageError::InvalidFences)
    );
}

#[test]
fn a_batch_that_does_not_fit_in_place_is_page_full_and_splits() {
    let mut bytes = base(128, b"a", b"z", &[b"b", b"c"]);
    let before = bytes.clone();
    let value = [b'v'; 24];
    let batch = [live(b"d", &value), live(b"e", &value), live(b"f", &value)];
    let page = Page::mut_from_bytes(&mut bytes).unwrap();
    assert_eq!(page.merge_batch_fits(batch.iter().copied()), Ok(false));
    assert_eq!(
        page.merge_batch_in_place(batch.iter().copied()),
        Err(PageError::PageFull)
    );
    assert_eq!(bytes, before);

    let page = Page::mut_from_bytes(&mut bytes).unwrap();
    let mut right = vec![0; 128];
    let split = page
        .merge_batch_split(&mut right, batch.iter().copied())
        .unwrap();
    let separator = split.separator.clone();
    let mut all = contents(page);
    all.extend(contents(split.right));
    let keys: Vec<_> = all.iter().map(|(key, _)| key.clone()).collect();
    assert_eq!(keys, [b"b", b"c", b"d", b"e", b"f"].map(|k| k.to_vec()));

    // The outputs tile the old fences at the separator.
    let left_fences = page.fences().unwrap();
    assert_eq!(left_fences.lower, Fence::Key(KeyRef::from(b"a".as_slice())));
    assert_eq!(
        left_fences.upper,
        Fence::Key(KeyRef::from(separator.as_slice()))
    );
    let right_page = Page::validate(&right).unwrap();
    let right_fences = right_page.fences().unwrap();
    assert_eq!(
        right_fences.lower,
        Fence::Key(KeyRef::from(separator.as_slice()))
    );
    assert_eq!(
        right_fences.upper,
        Fence::Key(KeyRef::from(b"z".as_slice()))
    );
    Page::validate(&bytes).unwrap();
}

/// Random base pages and batches against a map: whenever the batch fits,
/// the in-place result equals the map and its used bytes equal the size
/// `merge_batch_fits` judged; otherwise the split outputs together equal it.
#[test]
fn merges_agree_with_a_map() {
    let mut seed = 99u64;
    let mut next = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };

    for round in 0..500 {
        let capacity = [128, 256, 512][round % 3];
        let lower = b"k".to_vec();
        let upper = b"kz".to_vec();
        let mut bytes = vec![0; capacity];
        let fences = Fences {
            lower: Fence::Key(lower.as_slice()),
            upper: Fence::Key(upper.as_slice()),
        };
        let page = Page::init_with_fences(&mut bytes, fences).unwrap();
        let mut oracle = BTreeMap::new();
        for _ in 0..next(12) {
            let key = vec![b'k', b'a' + next(20) as u8];
            let value = vec![b'b'; next(8) as usize];
            if page.try_insert(&key, &value).is_ok() {
                oracle.insert(key, value);
            }
        }

        let mut batch_keys: Vec<Vec<u8>> = (0..next(10))
            .map(|_| vec![b'k', b'a' + next(20) as u8])
            .collect();
        batch_keys.sort();
        batch_keys.dedup();
        let values: Vec<Vec<u8>> = batch_keys
            .iter()
            .map(|_| vec![b'n'; next(12) as usize])
            .collect();
        let deletes: Vec<bool> = batch_keys.iter().map(|_| next(3) == 0).collect();
        let batch: Vec<_> = batch_keys
            .iter()
            .zip(&values)
            .zip(&deletes)
            .map(
                |((key, value), &delete)| {
                    if delete { dead(key) } else { live(key, value) }
                },
            )
            .collect();
        for ((key, value), &delete) in batch_keys.iter().zip(&values).zip(&deletes) {
            if delete {
                oracle.remove(key);
            } else {
                oracle.insert(key.clone(), value.clone());
            }
        }
        let expected: Vec<_> = oracle.into_iter().collect();

        let page = Page::mut_from_bytes(&mut bytes).unwrap();
        let fits = page.merge_batch_fits(batch.iter().copied()).unwrap();
        if fits {
            page.merge_batch_in_place(batch.iter().copied()).unwrap();
            assert_eq!(contents(page), expected, "round {round}");
            let fence_bytes = capacity - page.record_capacity();
            let records: usize = expected
                .iter()
                .map(|(key, value)| 8 + key.len() - 1 + value.len())
                .sum();
            assert_eq!(page.capacity() - page.free_bytes(), fence_bytes + records);
            Page::validate(&bytes).unwrap();
        } else {
            let mut right = vec![0; capacity];
            match page.merge_batch_split(&mut right, batch.iter().copied()) {
                Ok(split) => {
                    let mut all = contents(page);
                    all.extend(contents(split.right));
                    assert_eq!(all, expected, "round {round}");
                    Page::validate(&bytes).unwrap();
                    Page::validate(&right).unwrap();
                }
                // Small pages with few large records may not split; nothing
                // may change then.
                Err(PageError::CannotSplit) => {}
                Err(error) => panic!("round {round}: {error}"),
            }
        }
    }
}
