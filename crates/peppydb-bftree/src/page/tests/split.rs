use super::prefix::compressed_fixture;
use super::*;
use crate::page::split::{Merge, choose_split};

type Owned = (Vec<u8>, RecordKind, Option<Vec<u8>>, bool);

fn owned(page: &Page) -> Vec<Owned> {
    page.iter()
        .map(|record| {
            (
                record.key.to_vec(),
                record.state.kind(),
                record.state.value().map(<[u8]>::to_vec),
                record.referenced,
            )
        })
        .collect()
}

fn owned_fences(page: &Page) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let fences = page.fences().unwrap();
    let own = |fence: Fence<KeyRef<'_>>| match fence {
        Fence::Unbounded => None,
        Fence::Key(key) => Some(key.to_vec()),
    };
    (own(fences.lower), own(fences.upper))
}

fn lcp(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Packed size of a fenced page from the documented formula, computed
/// independently of the production split code.
fn packed_size(lower: Option<&[u8]>, upper: Option<&[u8]>, records: &[Owned]) -> usize {
    let prefix = match (lower, upper) {
        (Some(lower), Some(upper)) => lcp(lower, upper),
        _ => 0,
    };
    let fences = 16 + lower.map_or(0, <[u8]>::len) + upper.map_or(0, |upper| upper.len() - prefix);
    let records: usize = records
        .iter()
        .map(|(key, _, value, _)| 8 + key.len() - prefix + value.as_ref().map_or(0, Vec::len))
        .sum();
    32 + fences + records
}

/// Score every split point exhaustively; return `(index, left, right)`.
fn expected_split(
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
    records: &[Owned],
    capacity: usize,
) -> Option<(usize, usize, usize)> {
    (1..records.len())
        .map(|index| {
            let key = records[index].0.as_slice();
            let left = packed_size(lower, Some(key), &records[..index]);
            let right = packed_size(Some(key), upper, &records[index..]);
            (index, left, right)
        })
        .filter(|&(_, left, right)| left <= capacity && right <= capacity)
        .min_by_key(|&(index, left, right)| (left.max(right), left.abs_diff(right), index))
}

fn fences<'a>(lower: Option<&'a [u8]>, upper: Option<&'a [u8]>) -> Fences<&'a [u8]> {
    Fences {
        lower: lower.map_or(Fence::Unbounded, Fence::Key),
        upper: upper.map_or(Fence::Unbounded, Fence::Key),
    }
}

/// Split `page` with a pending record and check the outputs against the
/// independent calculator, the fences, and record conservation. Return the
/// separator and the right output's bytes.
fn split_and_check(page: &mut Page, key: &[u8], state: RecordState<'_>) -> (Vec<u8>, Vec<u8>) {
    let capacity = page.capacity();
    let (lower, upper) = owned_fences(page);
    let mut expected = owned(page);
    let position = expected.partition_point(|record| record.0.as_slice() < key);
    let pending = (
        key.to_vec(),
        state.kind(),
        state.value().map(<[u8]>::to_vec),
        false,
    );
    expected.insert(position, pending);
    let (index, left_used, right_used) =
        expected_split(lower.as_deref(), upper.as_deref(), &expected, capacity).unwrap();

    let mut right_storage = vec![0xa5; capacity];
    let split = page
        .split_with_insert(&mut right_storage, key, state)
        .unwrap();
    assert_eq!(split.separator, expected[index].0);
    assert_eq!(owned(page), expected[..index]);
    assert_eq!(owned(split.right), expected[index..]);
    assert_eq!(owned_fences(page), (lower, Some(split.separator.clone())));
    assert_eq!(
        owned_fences(split.right),
        (Some(split.separator.clone()), upper)
    );
    assert_eq!(page.capacity() - page.free_bytes(), left_used);
    assert_eq!(
        split.right.capacity() - split.right.free_bytes(),
        right_used
    );
    assert_eq!(page.reclaimable_bytes(), 0);
    assert_eq!(split.right.reclaimable_bytes(), 0);
    (split.separator, right_storage)
}

#[test]
fn pending_record_before_inside_or_after_existing_records() {
    for pending in [&b"a"[..], b"c", b"e", b"g", b"i"] {
        let mut storage = [0; 256];
        let page = Page::init_with_fences(&mut storage, fences(None, None)).unwrap();
        for key in [b"b", b"d", b"f", b"h"] {
            page.try_insert(key, b"value").unwrap();
        }
        let (separator, _) = split_and_check(page, pending, RecordState::Insert(b"value"));
        // Equal sizes split the five records 2 + 3 or 3 + 2; ties favor the
        // smaller fence, which belongs to the lower index.
        assert!(page.len() == 2 || page.len() == 3, "{pending:?}");
        assert!(KeyRef::from(separator.as_slice()) > page.iter().last().unwrap().key);
    }
}

#[test]
fn separator_is_right_lower_fence_and_routes_agree() {
    let mut storage = [0; 128];
    let page = Page::init_with_fences(&mut storage, fences(Some(b"a"), Some(b"z"))).unwrap();
    for key in [b"b", b"d", b"f"] {
        page.try_insert(key, &[0; 12]).unwrap();
    }
    let mut right_storage = [0; 128];
    let split = page
        .split_with_insert(&mut right_storage, b"e", RecordState::Insert(&[0; 12]))
        .unwrap();
    let separator = split.separator.clone();
    // Every key routes to exactly the side whose fences accept it.
    for key in [&b"a"[..], b"b", b"c", b"d", b"dz", b"e", b"f", b"y"] {
        let right = key >= separator.as_slice();
        assert_eq!(page.get(key).is_ok(), !right, "{key:?}");
        assert_eq!(split.right.get(key).is_ok(), right, "{key:?}");
    }
    assert_eq!(split.right.get(b"z"), Err(PageError::KeyOutOfRange));
}

#[test]
fn prefixes_are_recomputed_for_each_output() {
    // The source fences share nothing, so the page stores no prefix.
    let mut storage = [0; 256];
    let page = Page::init_with_fences(&mut storage, fences(Some(b"k"), Some(b"l"))).unwrap();
    assert_eq!(read_u16(page.as_bytes(), 6), 0);
    for key in [&b"kaaa1"[..], b"kaaa2", b"kbbb1", b"kbbb2"] {
        page.try_insert(key, b"v").unwrap();
    }
    let (separator, _) = split_and_check(page, b"kaaa3", RecordState::Insert(b"v"));
    assert_eq!(separator, b"kbbb1");
    // Left `[k, kbbb1)` shares `k`; right `[kbbb1, l)` shares nothing.
    assert_eq!(read_u16(page.as_bytes(), 6), 1);

    // A source that under-uses its fence prefix still yields full output prefixes.
    let records = [
        (&b"acct/120"[..], RecordState::Insert(b"x"), false),
        (b"acct/130", RecordState::Insert(b"x"), false),
        (b"acct/170", RecordState::Insert(b"x"), true),
    ];
    for stored_prefix in [0, 3, 5] {
        let mut storage = [0; 128];
        let page = compressed_fixture(
            &mut storage,
            b"acct/100",
            b"acct/200",
            stored_prefix,
            &records,
        );
        let mut right_storage = [0; 128];
        let split = page
            .split_with_insert(&mut right_storage, b"acct/180", RecordState::Insert(b"x"))
            .unwrap();
        assert_eq!(split.separator, b"acct/170");
        // `[acct/100, acct/170)` shares `acct/1`; `[acct/170, acct/200)` shares `acct/`.
        assert_eq!(read_u16(page.as_bytes(), 6), 6);
        assert_eq!(read_u16(split.right.as_bytes(), 6), 5);
        assert!(split.right.iter().next().unwrap().referenced);
    }
}

#[test]
fn record_kinds_reference_bits_and_header_metadata_are_preserved() {
    let mut storage = [0; 256];
    let page = Page::init_with_fences(&mut storage, fences(Some(b"a"), None)).unwrap();
    let states = [
        (b"b", RecordState::Insert(b"live")),
        (b"c", RecordState::Delete),
        (b"d", RecordState::Cache(b"cached")),
        (b"e", RecordState::Phantom),
        (b"f", RecordState::Insert(b"")),
    ];
    for (key, state) in states {
        page.try_insert_record(key, state).unwrap();
    }
    for key in [b"c", b"d"] {
        page.set_referenced(key, true).unwrap();
    }
    // A retained tombstone value is dropped by the rebuild.
    page.try_upsert(b"f", RecordState::Insert(b"retained"))
        .unwrap();
    page.try_upsert(b"f", RecordState::Delete).unwrap();
    page.as_mut_bytes()[8..32].fill(0x5a);

    let (_, right) = split_and_check(page, b"g", RecordState::Phantom);
    assert_eq!(&page.as_bytes()[8..32], [0x5a; 24]);
    assert_eq!(&right[8..32], [0x5a; 24]);
    let right = Page::ref_from_bytes(&right).unwrap();
    assert!(right.iter().chain(page.iter()).all(|record| {
        record.referenced
            == (record.key.cmp_bytes(b"c").is_eq() || record.key.cmp_bytes(b"d").is_eq())
    }));
}

#[test]
fn errors_follow_precedence_and_preserve_both_buffers() {
    let mut storage = [0; 128];
    let page = Page::init_with_fences(&mut storage, fences(Some(b"b"), Some(b"y"))).unwrap();
    page.try_insert(b"c", b"1").unwrap();
    page.try_insert(b"x", b"2").unwrap();
    let snapshot = page.as_bytes().to_vec();
    let too_large = [7; 128];

    let cases: [(usize, &[u8], &[u8], PageError); 7] = [
        (96, b"", b"", PageError::InvalidCapacity),
        (64, b"", b"", PageError::CapacityMismatch),
        (128, b"", b"", PageError::EmptyKey),
        (128, b"a", b"", PageError::KeyOutOfRange),
        (128, b"y", b"", PageError::KeyOutOfRange),
        (128, b"d", &too_large, PageError::RecordTooLarge),
        (128, b"c", b"", PageError::DuplicateKey),
    ];
    for (capacity, key, value, error) in cases {
        let mut right_storage = vec![0xa5; capacity];
        let result = page.split_with_insert(&mut right_storage, key, RecordState::Insert(value));
        assert_eq!(result.err(), Some(error), "{key:?}");
        assert_eq!(page.as_bytes(), snapshot);
        assert_eq!(right_storage, vec![0xa5; capacity]);
    }

    // An unfenced page has no bounds to divide.
    let mut storage = [0; 128];
    let page = Page::init(&mut storage).unwrap();
    page.try_insert(b"a", b"1").unwrap();
    let snapshot = page.as_bytes().to_vec();
    let mut right_storage = [0xa5; 128];
    let result = page.split_with_insert(&mut right_storage, b"b", RecordState::Insert(b""));
    assert_eq!(result.err(), Some(PageError::InvalidFences));
    assert_eq!(page.as_bytes(), snapshot);
    assert_eq!(right_storage, [0xa5; 128]);
}

#[test]
fn cannot_split_one_record_or_oversized_outputs() {
    // A single virtual record cannot leave a record on each side.
    let mut storage = [0; 64];
    let page = Page::init_with_fences(&mut storage, fences(None, None)).unwrap();
    let mut right_storage = [0xa5; 64];
    let result = page.split_with_insert(&mut right_storage, b"a", RecordState::Insert(b""));
    assert_eq!(result.err(), Some(PageError::CannotSplit));
    assert_eq!(right_storage, [0xa5; 64]);

    // Long keys make the only candidate's right output exceed 64 bytes.
    let mut storage = [0; 64];
    let page = Page::init_with_fences(&mut storage, fences(None, None)).unwrap();
    page.try_insert(&[b'a'; 8], b"").unwrap();
    let snapshot = page.as_bytes().to_vec();
    let result = page.split_with_insert(&mut right_storage, &[b'b'; 8], RecordState::Insert(b""));
    // Right output: 32 + 16 + 8 (low fence) + 8 + 8 = 72 bytes.
    assert_eq!(result.err(), Some(PageError::CannotSplit));
    assert_eq!(page.as_bytes(), snapshot);
    assert_eq!(right_storage, [0xa5; 64]);
}

#[test]
fn merge_orders_pending_records_and_replaces_equal_keys() {
    let mut storage = [0; 256];
    let page = Page::init_with_fences(&mut storage, fences(Some(b"a"), Some(b"z"))).unwrap();
    for key in [b"b", b"d", b"f"] {
        page.try_insert(key, b"old").unwrap();
    }
    let pending = [
        live(b"a", b"new"),
        live(b"d", b"new"),
        live(b"e", b"new"),
        live(b"y", b"new"),
    ];
    let merged: Vec<_> = Merge::new(page.iter(), &pending)
        .map(|record| (record.key.to_vec(), record.state))
        .collect();
    let expected: Vec<(Vec<u8>, RecordState<'_>)> = [
        (b"a", b"new"),
        (b"b", b"old"),
        (b"d", b"new"),
        (b"e", b"new"),
        (b"f", b"old"),
        (b"y", b"new"),
    ]
    .iter()
    .map(|&(key, value)| (key.to_vec(), RecordState::Insert(value.as_slice())))
    .collect();
    assert_eq!(merged, expected);

    // A batch can drive the same selector as a single pending record.
    let fences = page.fences().unwrap();
    let index = choose_split(Merge::new(page.iter(), &pending), fences, 0, 256);
    assert_eq!(index, Some(3));
}

#[test]
fn random_pages_split_exactly_as_the_calculator_predicts() {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move |bound: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % bound as u64) as usize
    };
    // Each profile: fences and the shared start of every generated key.
    type Profile = (Option<&'static [u8]>, Option<&'static [u8]>, &'static [u8]);
    let profiles: [Profile; 5] = [
        (None, None, b"k"),
        (Some(b"k"), None, b"k"),
        (None, Some(b"l"), b"k"),
        (Some(b"k"), Some(b"l"), b"k"),
        (Some(b"user/"), Some(b"user/\xff"), b"user/"),
    ];
    for capacity in [128, 256, 512, 4096] {
        for (lower, upper, start) in profiles {
            for _ in 0..40 {
                let mut storage = vec![0; capacity];
                let page = Page::init_with_fences(&mut storage, fences(lower, upper)).unwrap();
                let max_value = capacity / 16;
                let (key, value) = loop {
                    let suffix: Vec<u8> = (0..1 + next(8)).map(|_| b'a' + next(3) as u8).collect();
                    let key = [start, suffix.as_slice()].concat();
                    let value = vec![next(256) as u8; next(max_value)];
                    match page.try_insert(&key, &value) {
                        Ok(()) => {
                            if next(4) == 0 {
                                page.set_referenced(&key, true).unwrap();
                            }
                        }
                        Err(PageError::DuplicateKey) => {}
                        Err(PageError::PageFull) => break (key, value),
                        Err(error) => panic!("{error}"),
                    }
                };
                let state = match next(4) {
                    0 => RecordState::Delete,
                    1 => RecordState::Cache(&value),
                    _ => RecordState::Insert(&value),
                };
                split_and_check(page, &key, state);
            }
        }
    }
}
