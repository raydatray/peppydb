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
        assert_eq!(page.get(key), Ok(Some(value.as_slice())));
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
        assert_eq!(page.get(key), Ok(Some(b"".as_slice())));
    }
    for (index, key) in sorted.into_iter().enumerate() {
        assert_eq!(page.record_at(index), (key, b"".as_slice()));
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
    assert_eq!(page.get(b"k"), Ok(Some(value.as_slice())));
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
