use super::*;

fn assert_model(page: &Page, model: &BTreeMap<Vec<u8>, Vec<u8>>) {
    let before = page.as_bytes().to_vec();
    let capacity = before.len();
    let payload_bytes: usize = model
        .iter()
        .map(|(key, value)| key.len() + value.len())
        .sum();
    let slots_end = 32 + model.len() * 8;
    let payload_start = capacity - payload_bytes;
    assert_eq!(page.len(), model.len());
    assert_eq!(page.is_empty(), model.is_empty());
    assert_eq!(page.capacity(), capacity);
    assert_eq!(page.free_bytes(), payload_start - slots_end);
    let actual: Vec<_> = page.iter().collect();
    let expected: Vec<_> = model.iter().map(|(key, value)| live(key, value)).collect();
    assert_eq!(actual, expected);
    for (key, value) in model {
        assert_eq!(page.get(key), Ok(Some(live(key, value))));
    }
    assert_eq!(page.get(b""), Err(PageError::EmptyKey));
    assert_eq!(page.get(b"not-present"), Ok(None));
    assert_eq!(page.get(b"\xff\xff"), Ok(None));

    // Decode independently of the production structs. State bits must be
    // zero for every record this API can create.
    let bytes = page.as_bytes();
    let word = |offset| u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
    assert_eq!(word(0), model.len() as u16);
    assert_eq!(word(2), capacity as u16);
    assert_eq!(usize::from(word(4)), payload_start - slots_end);
    assert_eq!(&bytes[6..8], &[0; 2]);
    assert_eq!(&bytes[8..16], &[0xff; 8]);
    assert_eq!(&bytes[16..32], &[0; 16]);
    let mut occupied = vec![false; capacity];
    occupied[..slots_end].fill(true);
    for (index, (key, value)) in model.iter().enumerate() {
        let slot = 32 + index * 8;
        let offset = usize::from(word(slot));
        let value_start = offset + key.len();
        let end = value_start + value.len();
        assert!(offset >= payload_start && end <= capacity);
        assert_eq!(word(slot + 2), key.len() as u16);
        assert_eq!(word(slot + 4), value.len() as u16);
        assert_eq!(
            &bytes[slot + 6..slot + 8],
            &[key[0], key.get(1).copied().unwrap_or(0)]
        );
        assert_eq!(&bytes[offset..value_start], key);
        assert_eq!(&bytes[value_start..end], value);
        assert!(occupied[offset..end].iter().all(|&used| !used));
        occupied[offset..end].fill(true);
    }
    assert!(occupied[slots_end..payload_start].iter().all(|&used| !used));
    assert!(occupied[payload_start..].iter().all(|&used| used));
    assert_eq!(page.as_bytes(), before);
}

fn model_insert(page: &mut Page, model: &mut BTreeMap<Vec<u8>, Vec<u8>>, key: &[u8], value: &[u8]) {
    let usable = page.as_bytes().len() - 32;
    let required = 8 + key.len() + value.len();
    let used: usize = model
        .iter()
        .map(|(key, value)| 8 + key.len() + value.len())
        .sum();
    let expected = if key.is_empty() {
        Err(PageError::EmptyKey)
    } else if required > usable {
        Err(PageError::RecordTooLarge)
    } else if model.contains_key(key) {
        Err(PageError::DuplicateKey)
    } else if used + required > usable {
        Err(PageError::PageFull)
    } else {
        Ok(())
    };
    let before = page.as_bytes().to_vec();
    assert_eq!(page.try_insert(key, value), expected);
    if expected.is_ok() {
        model.insert(key.to_vec(), value.to_vec());
    } else {
        assert_eq!(page.as_bytes(), before);
    }
    if !key.is_empty() {
        assert_eq!(
            page.get(key),
            Ok(model.get(key).map(|value| live(key, value)))
        );
    }
    assert_model(page, model);
}

#[test]
fn deterministic_sequences_match_ordered_map() {
    // Powers of two, upstream's default mini-page sizes that are not
    // powers of two, and 4032, the largest such supported capacity.
    for capacity in [64, 128, 256, 448, 512, 832, 1024, 1600, 2048, 4032, 4096] {
        let count = (capacity / 10 + 16).min(420);
        let mut keys: Vec<_> = (0..count as u16)
            .map(|index| index.to_be_bytes().to_vec())
            .collect();
        keys.extend([
            b"\0".to_vec(),
            b"\0\0\0".to_vec(),
            b"ab".to_vec(),
            b"abc".to_vec(),
            b"\xff".to_vec(),
        ]);
        keys.sort();
        for order in 0..3 {
            let mut indices: Vec<_> = (0..keys.len()).collect();
            match order {
                0 => {}
                1 => indices.reverse(),
                _ => {
                    indices.sort_unstable_by_key(|&index| (index as u32).wrapping_mul(0x9e37_79b9))
                }
            }
            for profile in 0..2 {
                let mut storage = vec![0xa5; capacity];
                let page = Page::init(&mut storage).unwrap();
                let mut model = BTreeMap::new();
                assert_model(page, &model);
                model_insert(page, &mut model, b"", b"");
                for &index in &indices {
                    let value_len = if profile == 0 {
                        0
                    } else {
                        [0, 1, 2, 7, 31, capacity / 4, capacity][index % 7]
                    };
                    let value: Vec<_> = (0..value_len).map(|byte| (byte ^ index) as u8).collect();
                    model_insert(page, &mut model, &keys[index], &value);
                    if index % 11 == 0 {
                        model_insert(page, &mut model, &keys[index], b"replacement");
                    }
                }
                let oversized = vec![0; capacity];
                model_insert(page, &mut model, &keys[0], &oversized);
                model_insert(page, &mut model, &oversized, b"");
                model_insert(page, &mut model, b"", &oversized);
                if profile == 0 && capacity == 4096 {
                    assert!(model.len() > 255, "exercise both count bytes");
                }
            }
        }
    }
}
