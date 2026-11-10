use std::collections::BTreeMap;

use bf_tree_page::{Fence, Fences, Page, PageError, RecordState};
use zerocopy::FromBytes;

use super::*;

fn fenced(capacity: usize, lower: &[u8], upper: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; capacity];
    let fences = Fences {
        lower: Fence::Key(lower),
        upper: Fence::Key(upper),
    };
    Page::init_with_fences(&mut bytes, fences).unwrap();
    bytes
}

#[test]
fn range_merges_base_and_mini_and_hides_absent_keys() {
    let mut base = fenced(512, b"a", b"z");
    let page = Page::mut_from_bytes(&mut base).unwrap();
    for (key, value) in [
        (&b"b"[..], &b"base"[..]),
        (b"d", b"base"),
        (b"f", b"base"),
        (b"h", b"base"),
    ] {
        page.try_insert(key, value).unwrap();
    }

    let mut mini_bytes = vec![0; 256];
    let mini = MiniPage::init(&mut mini_bytes, leaf(1));
    let mut refs = RefBits::default();
    let records = [
        (&b"c"[..], RecordState::Insert(b"mini")),
        (b"d", RecordState::Delete),
        (b"f", RecordState::Cache(b"base")),
        (b"g", RecordState::Phantom),
        (b"h", RecordState::Insert(b"new")),
    ];
    for (key, state) in records {
        mini.apply(&mut refs, key, state).unwrap();
    }

    let leaf_ref = LeafRef {
        base: Page::validate(&base).unwrap(),
        mini: Some(MiniPage::from_bytes(&mini_bytes)),
    };
    let all: Vec<_> = leaf_ref
        .range(None, None)
        .unwrap()
        .map(|r| (r.key.to_vec(), r.state.value().unwrap().to_vec()))
        .collect();
    let expected = [
        (b"b", &b"base"[..]),
        (b"c", b"mini"),
        (b"f", b"base"),
        (b"h", b"new"),
    ]
    .map(|(k, v)| (k.to_vec(), v.to_vec()));
    assert_eq!(all, expected);

    // Bounds apply to both sides.
    let bounded: Vec<_> = leaf_ref
        .range(Some(b"c"), Some(b"g"))
        .unwrap()
        .map(|r| r.key.to_vec())
        .collect();
    assert_eq!(bounded, [b"c".to_vec(), b"f".to_vec()]);
    assert!(matches!(
        leaf_ref.range(Some(b"g"), Some(b"c")),
        Err(PageError::InvalidRange)
    ));

    // Without a mini-page, the base alone.
    let base_only = LeafRef {
        mini: None,
        ..leaf_ref
    };
    assert_eq!(base_only.range(None, None).unwrap().count(), 4);
}

#[test]
fn merges_apply_dirty_records_and_drop_clean_ones() {
    let mut base = fenced(512, b"a", b"z");
    let page = Page::mut_from_bytes(&mut base).unwrap();
    page.try_insert(b"b", b"base").unwrap();
    page.try_insert(b"d", b"base").unwrap();

    let mut mini_bytes = vec![0; 256];
    let mini = MiniPage::init(&mut mini_bytes, leaf(1));
    let mut refs = RefBits::default();
    mini.apply(&mut refs, b"c", RecordState::Insert(b"new"))
        .unwrap();
    mini.apply(&mut refs, b"d", RecordState::Delete).unwrap();
    mini.apply(&mut refs, b"e", RecordState::Cache(b"clean"))
        .unwrap();

    let mini = MiniPage::from_bytes(&mini_bytes);
    let mut leaf_mut = LeafMut {
        base: Page::mut_from_bytes(&mut base).unwrap(),
        mini: Some(mini),
    };
    assert_eq!(leaf_mut.merge_fits(), Ok(true));
    leaf_mut.merge_in_place().unwrap();
    let keys: Vec<_> = Page::validate(&base)
        .unwrap()
        .iter()
        .map(|r| r.key.to_vec())
        .collect();
    assert_eq!(keys, [b"b".to_vec(), b"c".to_vec()]);
}

/// Keys of length `len` that sort between the fences `[0x01; len]` and
/// `[0x02; len]`, so the fences share no prefix: the worst case for fence
/// bytes.
fn key(index: usize, len: usize) -> Vec<u8> {
    // [0x01, 0x80, ...] is above the lower fence and below the upper one; the
    // counter fills the remaining bytes, so keys sort by index.
    let mut key = vec![0x01; len];
    key[1] = 0x80;
    let bytes = (index as u64).to_be_bytes();
    let tail = (len - 2).min(8);
    key[len - tail..].copy_from_slice(&bytes[8 - tail..]);
    key
}

/// The admission rule: a full base page plus a full largest-class mini-page
/// of maximum records merges in place or with one two-way split, in every
/// test configuration, and never returns `CannotSplit`.
#[test]
fn a_full_base_and_a_full_largest_mini_page_always_merge() {
    for (l, k, v) in [
        (4096, 64, 1024),
        (512, 16, 32),
        (128, 8, 0),
        (1024, 32, 100),
    ] {
        let classes = SizeClasses::derive(l, k, v).unwrap();
        let m = classes.largest();
        for interleave in [false, true] {
            let mut base = fenced(l, &vec![0x01; k], &vec![0x02; k]);
            let page = Page::mut_from_bytes(&mut base).unwrap();
            let value = vec![0xbb; v];
            let mut oracle = BTreeMap::new();
            let mut index = 0;
            loop {
                let key = key(if interleave { index * 2 } else { index }, k);
                if page.try_insert(&key, &value).is_err() {
                    break;
                }
                oracle.insert(key, value.clone());
                index += 1;
            }

            let mut mini_bytes = vec![0; m];
            let mini = MiniPage::init(&mut mini_bytes, leaf(1));
            let mut refs = RefBits::default();
            let new_value = vec![0xcc; v];
            let mut index = 0;
            loop {
                let key = key(
                    if interleave {
                        index * 2 + 1
                    } else {
                        10_000 + index
                    },
                    k,
                );
                if mini
                    .apply(&mut refs, &key, RecordState::Insert(&new_value))
                    .is_err()
                {
                    break;
                }
                oracle.insert(key, new_value.clone());
                index += 1;
            }
            assert!(index > 0, "the largest class holds a maximum record");

            let mut leaf_mut = LeafMut {
                base: Page::mut_from_bytes(&mut base).unwrap(),
                mini: Some(MiniPage::from_bytes(&mini_bytes)),
            };
            let mut right = vec![0; l];
            let mut all: Vec<_> = Vec::new();
            // The base is full and the mini-page adds distinct records, so
            // the merge must split.
            assert!(!leaf_mut.merge_fits().unwrap());
            {
                let split = leaf_mut
                    .merge_split(&mut right)
                    .unwrap_or_else(|e| panic!("({l}, {k}, {v}): {e}"));
                all.extend(
                    split
                        .right
                        .iter()
                        .map(|r| (r.key.to_vec(), r.state.value().unwrap().to_vec())),
                );
            }
            let mut left: Vec<_> = Page::validate(&base)
                .unwrap()
                .iter()
                .map(|r| (r.key.to_vec(), r.state.value().unwrap().to_vec()))
                .collect();
            left.extend(all);
            assert_eq!(
                left,
                oracle.into_iter().collect::<Vec<_>>(),
                "({l}, {k}, {v})"
            );
        }
    }
}
