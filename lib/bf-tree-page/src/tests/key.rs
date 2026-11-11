use std::cmp::Ordering;

use super::*;

/// Byte strings covering empty keys, prefixes of each other, embedded and
/// trailing zero bytes, high bytes, and ties broken only by length.
fn samples() -> Vec<Vec<u8>> {
    let mut samples: Vec<Vec<u8>> = [
        b"".as_slice(),
        b"\0",
        b"\0\0",
        b"\0\x01",
        b"a",
        b"a\0",
        b"ab",
        b"abc",
        b"abd",
        b"abc\0",
        b"b",
        b"\x7f",
        b"\x80",
        b"\xff",
        b"\xff\xff",
    ]
    .into_iter()
    .map(<[u8]>::to_vec)
    .collect();
    samples.sort();
    samples
}

/// Every split of `key` into a prefix and suffix, including empty parts.
fn splits(key: &[u8]) -> impl Iterator<Item = KeyRef<'_>> {
    (0..=key.len()).map(|at| {
        let (prefix, suffix) = key.split_at(at);
        KeyRef::new(prefix, suffix)
    })
}

#[test]
fn accessors_describe_the_logical_key_and_keep_the_borrowed_lifetime() {
    let storage = b"acct/123".to_vec();
    let (prefix, suffix) = {
        // The view is a temporary; its parts still borrow `storage`.
        let key = KeyRef::new(&storage[..5], &storage[5..]);
        (key.prefix(), key.suffix())
    };
    assert_eq!(prefix, b"acct/");
    assert_eq!(suffix, b"123");
    assert_eq!(prefix.as_ptr(), storage.as_ptr());
    assert_eq!(suffix.as_ptr(), storage[5..].as_ptr());

    let bytes = KeyRef::new(&storage[..5], &storage[5..]).bytes();
    let collected: Vec<_> = bytes.clone().collect();
    assert_eq!(collected, storage);
    assert_eq!(bytes.clone().next_back(), Some(b'3'));

    let key = KeyRef::new(b"acct/", b"123");
    assert_eq!(key.len(), 8);
    assert!(!key.is_empty());
    assert_eq!(key.to_vec(), b"acct/123");
    for empty in [KeyRef::new(b"", b""), KeyRef::from(b"".as_slice())] {
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.bytes().count(), 0);
        assert_eq!(empty.to_vec(), b"");
    }
    let whole = KeyRef::from(storage.as_slice());
    assert_eq!(whole.prefix(), b"");
    assert_eq!(whole.suffix().as_ptr(), storage.as_ptr());
}

#[test]
fn comparisons_ignore_the_split_point() {
    let samples = samples();
    for left in &samples {
        for right in &samples {
            let expected = left.cmp(right);
            for left_view in splits(left) {
                assert_eq!(
                    left_view.cmp_bytes(right),
                    expected,
                    "{left_view:?} vs {right:?}"
                );
                for right_view in splits(right) {
                    assert_eq!(left_view.cmp(&right_view), expected);
                    assert_eq!(left_view.partial_cmp(&right_view), Some(expected));
                    assert_eq!(left_view == right_view, expected == Ordering::Equal);
                    assert_eq!(left_view.bytes().cmp(right_view.bytes()), expected);
                }
            }
        }
    }
}

#[test]
fn different_segmentations_of_one_key_are_equal() {
    assert_eq!(KeyRef::new(b"ab", b"c"), KeyRef::new(b"a", b"bc"));
    assert_eq!(KeyRef::new(b"abc", b""), KeyRef::from(b"abc".as_slice()));
    assert_ne!(KeyRef::new(b"ab", b""), KeyRef::new(b"ab", b"\0"));
    assert!(KeyRef::new(b"ab", b"") < KeyRef::new(b"a", b"b\0"));
    assert!(KeyRef::new(b"abc", b"") > KeyRef::new(b"", b"ab"));
    // A prefix longer than the query compares greater when the query is its prefix.
    assert_eq!(KeyRef::new(b"abc", b"").cmp_bytes(b"ab"), Ordering::Greater);
    assert_eq!(KeyRef::new(b"abc", b"").cmp_bytes(b"abd"), Ordering::Less);
    assert_eq!(KeyRef::new(b"ab", b"").cmp_bytes(b""), Ordering::Greater);
    assert_eq!(KeyRef::new(b"", b"").cmp_bytes(b""), Ordering::Equal);
}

/// Every pair of strings up to length 3 over {0, 1, 255}, plus pseudo-random
/// longer pairs, orders the same under `cmp_short` as under slice order.
#[test]
fn cmp_short_matches_slice_order() {
    let alphabet = [0u8, 1, 255];
    let mut strings = vec![Vec::new()];
    for len in 1..=3u32 {
        strings.extend((0..alphabet.len().pow(len)).map(|mut n| {
            (0..len)
                .map(|_| {
                    let byte = alphabet[n % alphabet.len()];
                    n /= alphabet.len();
                    byte
                })
                .collect()
        }));
    }
    for a in &strings {
        for b in &strings {
            assert_eq!(crate::key::cmp_short(a, b), a.cmp(b), "{a:?} {b:?}");
        }
    }
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let mut string = || -> Vec<u8> {
            let len = (next() % 20) as usize;
            // A small alphabet makes long common prefixes likely.
            (0..len).map(|_| alphabet[(next() % 3) as usize]).collect()
        };
        let (a, b) = (string(), string());
        assert_eq!(crate::key::cmp_short(&a, &b), a.cmp(&b), "{a:?} {b:?}");
        let split = (next() as usize) % (a.len() + 1);
        let key = KeyRef::new(&a[..split], &a[split..]);
        assert_eq!(key.cmp_bytes(&b), a.cmp(&b), "{a:?} split {split} vs {b:?}");
    }
}
