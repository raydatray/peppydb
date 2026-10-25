use super::*;
use crate::page::layout::{HAS_FENCES, HIGH_INFINITY, LOW_INFINITY};

struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

fn key(rng: &mut Rng, prefix: &[u8], max_tail: usize) -> Vec<u8> {
    let tail = (1 + rng.below(max_tail)).min(8);
    prefix
        .iter()
        .copied()
        .chain((0..tail).map(|_| b"abcz\0\xff"[rng.below(6)]))
        .collect()
}

/// Pages produced by every mutation path this crate has: unfenced, one-sided,
/// and prefix-compressed fences; inserts, upserts, removes, tombstones,
/// compaction, and both outputs of a split.
fn model_pages(seed: u64) -> Vec<Vec<u8>> {
    let mut rng = Rng(seed);
    let mut pages = Vec::new();
    type Bounds = (Option<&'static [u8]>, Option<&'static [u8]>, &'static [u8]);
    let bounds: [Bounds; 4] = [
        (None, None, b""),
        (None, Some(b"m"), b""),
        (Some(b"k"), None, b""),
        (Some(b"tenant/a"), Some(b"tenant/z"), b"tenant/"),
    ];
    for capacity in [64, 128, 256, 1024] {
        for (lower, upper, prefix) in bounds {
            let mut bytes = vec![0; capacity];
            let fences = Fences {
                lower: lower.map_or(Fence::Unbounded, Fence::Key),
                upper: upper.map_or(Fence::Unbounded, Fence::Key),
            };
            let page = Page::init_with_fences(&mut bytes, fences).unwrap();
            let mut keys = Vec::new();
            for step in 0..40 {
                let key = key(&mut rng, prefix, 3);
                let in_range = lower.is_none_or(|lower| key.as_slice() >= lower)
                    && upper.is_none_or(|upper| key.as_slice() < upper);
                if !in_range {
                    continue;
                }
                let value = vec![step as u8; rng.below(6)];
                match rng.below(5) {
                    0 => {
                        let _ = page.remove(&key);
                    }
                    1 => {
                        let _ = page.try_upsert(&key, RecordState::Delete);
                    }
                    2 if rng.below(3) == 0 => {
                        page.compact();
                    }
                    _ => {
                        if page.try_upsert(&key, RecordState::Insert(&value)).is_ok() {
                            keys.push(key);
                        }
                    }
                }
            }
            if page.len() >= 2 {
                let mut right = vec![0; capacity];
                let mut left = bytes.clone();
                let left_page = Page::mut_from_bytes(&mut left).unwrap();
                let pending = key(&mut rng, prefix, 3);
                let in_range = lower.is_none_or(|lower| pending.as_slice() >= lower)
                    && upper.is_none_or(|upper| pending.as_slice() < upper);
                if in_range
                    && let Ok(split) =
                        left_page.split_with_insert(&mut right, &pending, RecordState::Insert(b"p"))
                {
                    drop(split);
                    pages.push(left);
                    pages.push(right);
                }
            }
            pages.push(bytes);
        }
    }
    let mut unfenced = vec![0; 128];
    let page = Page::init(&mut unfenced).unwrap();
    page.try_insert(b"b", b"1").unwrap();
    page.try_insert(b"a", b"").unwrap();
    pages.push(unfenced);

    pages
}

/// Exercise the trusted-bytes methods; any panic fails the test.
fn exercise(bytes: &[u8]) {
    let page = Page::validate(bytes).unwrap();
    let records: Vec<_> = page.iter().collect();
    for record in &records {
        assert_eq!(page.get(&record.key.to_vec()), Ok(Some(*record)));
    }
    let _ = page.fences();
    let _ = page.reclaimable_bytes();
    let _ = page.range(Some(b"a".as_slice()), Some(b"zz".as_slice()));

    let mut copy = bytes.to_vec();
    let page = Page::mut_from_bytes(&mut copy).unwrap();
    page.compact();
    let _ = page.try_upsert(b"q", RecordState::Insert(b"v"));
    if let Some(record) = records.first() {
        let _ = page.remove(&record.key.to_vec());
    }
    assert!(Page::validate(&copy).is_ok(), "mutation preserves validity");
}

#[test]
fn every_model_page_validates_and_stays_valid_under_mutation() {
    let pages = model_pages(0x5eed);
    assert!(pages.len() > 20);
    for bytes in &pages {
        exercise(bytes);
    }
}

#[test]
fn corrupted_images_never_panic_and_accepted_images_are_safe() {
    let mut rng = Rng(0xc0ffee);
    let mut rejected = 0;
    let mut accepted = 0;
    for bytes in model_pages(0xabcdef) {
        for _ in 0..300 {
            let mut image = bytes.clone();
            match rng.below(4) {
                0 => {
                    let index = rng.below(image.len());
                    image[index] ^= 1 << rng.below(8);
                }
                1 => {
                    for _ in 0..1 + rng.below(4) {
                        let index = rng.below(image.len());
                        image[index] = rng.below(256) as u8;
                    }
                }
                2 => {
                    // Header-focused: most structure lives in the first 48 bytes.
                    let index = rng.below(48.min(image.len()));
                    image[index] = rng.below(256) as u8;
                }
                _ => image.truncate(rng.below(image.len() + 1)),
            }
            match Page::validate(&image) {
                Ok(_) => {
                    accepted += 1;
                    exercise(&image);
                }
                Err(PageError::Corrupt(_)) => rejected += 1,
                Err(other) => panic!("unexpected error {other:?}"),
            }
        }
    }
    assert!(rejected > 0);
    assert!(accepted > 0, "benign flips (payload bytes, lsn) stay valid");
}

fn fenced_fixture() -> Vec<u8> {
    let mut bytes = vec![0; 128];
    let fences = Fences {
        lower: Fence::Key(b"ka".as_slice()),
        upper: Fence::Key(b"kz".as_slice()),
    };
    let page = Page::init_with_fences(&mut bytes, fences).unwrap();
    page.try_insert(b"kb", b"1").unwrap();
    page.try_insert(b"km", b"22").unwrap();
    bytes
}

fn set_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn reason(bytes: &[u8]) -> &'static str {
    match Page::validate(bytes) {
        Err(PageError::Corrupt(reason)) => reason,
        other => panic!("expected a corrupt page, got {other:?}"),
    }
}

#[test]
fn each_check_rejects_its_targeted_corruption() {
    let valid = fenced_fixture();
    assert!(Page::validate(&valid).is_ok());
    // Slot 0 (lower fence) at 32, slot 1 (upper) at 40, records at 48 and 56.
    assert_eq!(read_u16(&valid, 32 + 6), u16::from_le_bytes(*b"ka"));

    assert_eq!(reason(&valid[..100]), "illegal capacity");
    assert_eq!(
        reason(&[0; 128]),
        "capacity field differs from the buffer length"
    );

    let mut image = valid.clone();
    let flags = read_u16(&image, 0) | 1 << 12;
    set_u16(&mut image, 0, flags);
    assert_eq!(reason(&image), "unknown header flags");

    let mut image = valid.clone();
    set_u16(&mut image, 0, HAS_FENCES | 13);
    assert_eq!(reason(&image), "slot directory does not fit");

    let mut image = valid.clone();
    set_u16(&mut image, 4, 200);
    assert_eq!(reason(&image), "free gap does not fit");

    let mut image = valid.clone();
    set_u16(&mut image, 0, HAS_FENCES | 1);
    assert_eq!(reason(&image), "fenced page has fewer than two slots");

    let mut image = valid.clone();
    set_u16(&mut image, 32 + 4, 1);
    assert_eq!(reason(&image), "fence slot carries a value");

    let mut image = valid.clone();
    set_u16(&mut image, 32, LOW_INFINITY);
    assert_eq!(reason(&image), "unbounded fence has a key length");

    let mut image = valid.clone();
    set_u16(&mut image, 32, LOW_INFINITY);
    set_u16(&mut image, 32 + 2, 0);
    assert_eq!(reason(&image), "prefix longer than the lower fence");

    let mut image = valid.clone();
    set_u16(&mut image, 48 + 2, 0);
    assert_eq!(reason(&image), "key shorter than the page prefix");

    let mut image = valid.clone();
    set_u16(&mut image, 48, 127);
    assert_eq!(reason(&image), "payload outside the payload area");

    let mut image = valid.clone();
    set_u16(&mut image, 48, 40);
    assert_eq!(reason(&image), "payload outside the payload area");

    let mut image = valid.clone();
    image[48 + 6] = b'x';
    assert_eq!(reason(&image), "slot preview differs from the stored key");

    let mut image = valid.clone();
    let first = read_u16(&image, 48);
    set_u16(&mut image, 56, first);
    image[56 + 6] = image[48 + 6];
    assert_eq!(reason(&image), "payloads overlap");

    let mut image = valid.clone();
    set_u16(&mut image, 40, HIGH_INFINITY);
    set_u16(&mut image, 40 + 2, 0);
    assert_eq!(reason(&image), "prefix with an unbounded fence");

    let mut image = valid.clone();
    let lower_offset = usize::from(read_u16(&image, 32));
    image[lower_offset + 1] = b'z';
    assert_eq!(reason(&image), "fences are not ordered");

    let mut image = valid.clone();
    let first_offset = usize::from(read_u16(&image, 48));
    image[first_offset] = b'n';
    image[48 + 6] = b'n';
    assert_eq!(reason(&image), "record keys are not strictly increasing");

    let mut image = valid.clone();
    image[first_offset] = b'`';
    image[48 + 6] = b'`';
    assert_eq!(reason(&image), "record below the lower fence");

    let mut image = valid.clone();
    let second_offset = usize::from(read_u16(&image, 56));
    image[second_offset] = b'z';
    image[56 + 6] = b'z';
    assert_eq!(reason(&image), "record at or above the upper fence");

    let mut unfenced = vec![0; 64];
    Page::init(&mut unfenced)
        .unwrap()
        .try_insert(b"a", b"")
        .unwrap();
    assert!(Page::validate(&unfenced).is_ok());
    let mut image = unfenced.clone();
    set_u16(&mut image, 32 + 2, 0);
    assert_eq!(reason(&image), "empty record key");
    set_u16(&mut unfenced, 6, 1);
    assert_eq!(reason(&unfenced), "prefix on an unfenced page");
}
