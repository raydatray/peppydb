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

// This model owns physical values, including bytes hidden by a tombstone. Its
// gap comes from operation history, not from page headers or layout helpers.
struct ModelRecord {
    kind: u16,
    stored: Vec<u8>,
    referenced: bool,
}

impl ModelRecord {
    fn new(state: RecordState<'_>) -> Self {
        let (kind, value) = match state {
            RecordState::Insert(value) => (0, value),
            RecordState::Delete => (1, b"".as_slice()),
            RecordState::Cache(value) => (2, value),
            RecordState::Phantom => (3, b"".as_slice()),
        };
        Self {
            kind,
            stored: value.to_vec(),
            referenced: false,
        }
    }

    fn borrow<'a>(&'a self, key: &'a [u8]) -> RecordRef<'a> {
        RecordRef {
            key,
            state: match self.kind {
                0 => RecordState::Insert(&self.stored),
                1 => RecordState::Delete,
                2 => RecordState::Cache(&self.stored),
                3 => RecordState::Phantom,
                _ => unreachable!(),
            },
            referenced: self.referenced,
        }
    }
}

struct MutationModel {
    records: BTreeMap<Vec<u8>, ModelRecord>,
    capacity: usize,
    fences: Option<Fences<'static>>,
    gap: usize,
}

impl MutationModel {
    fn new(capacity: usize, fences: Option<Fences<'static>>) -> Self {
        let mut model = Self {
            records: BTreeMap::new(),
            capacity,
            fences,
            gap: 0,
        };
        model.gap = capacity - model.structural_bytes();
        model
    }

    fn structural_bytes(&self) -> usize {
        32 + self.fences.map_or(0, |fences| {
            let payload: usize = [fences.lower, fences.upper]
                .iter()
                .map(|fence| match fence {
                    Fence::Key(key) => key.len(),
                    Fence::Unbounded => 0,
                })
                .sum();
            16 + payload
        })
    }

    fn key_error(&self, key: &[u8]) -> Option<PageError> {
        if key.is_empty() {
            return Some(PageError::EmptyKey);
        }
        if self.fences.is_some_and(|fences| {
            matches!(fences.lower, Fence::Key(lower) if key < lower)
                || matches!(fences.upper, Fence::Key(upper) if key >= upper)
        }) {
            Some(PageError::KeyOutOfRange)
        } else {
            None
        }
    }

    fn packed_bytes(&self) -> usize {
        let records: usize = self
            .records
            .iter()
            .map(|(key, record)| 8 + key.len() + record.stored.len())
            .sum();
        self.structural_bytes() + records
    }

    fn write(&mut self, page: &mut Page, key: &[u8], state: RecordState<'_>, distinct: bool) {
        let mut incoming = ModelRecord::new(state);
        let old = self.records.get(key);
        let required = 8 + key.len() + incoming.stored.len();
        let allocation = old.map_or(required, |old| {
            if incoming.kind != 1 && incoming.stored.len() > old.stored.len() {
                required - 8
            } else {
                0
            }
        });
        let expected = if let Some(error) = self.key_error(key) {
            Err(error)
        } else if required > self.capacity - self.structural_bytes() {
            Err(PageError::RecordTooLarge)
        } else if distinct && old.is_some() {
            Err(PageError::DuplicateKey)
        } else if allocation > self.gap {
            Err(PageError::PageFull)
        } else {
            Ok(if old.is_some() {
                UpsertOutcome::Replaced
            } else {
                UpsertOutcome::Inserted
            })
        };
        let before = page.as_bytes().to_vec();
        let actual = if distinct {
            page.try_insert_record(key, state)
                .map(|()| UpsertOutcome::Inserted)
        } else {
            page.try_upsert(key, state)
        };
        assert_eq!(
            actual,
            expected,
            "key={key:?}, kind={}, length={}, gap={}, distinct={distinct}",
            incoming.kind,
            incoming.stored.len(),
            self.gap
        );
        if actual.is_ok() {
            if let Some(old) = old {
                incoming.referenced = old.referenced;
                if incoming.kind == 1 {
                    incoming.stored.clone_from(&old.stored);
                }
            }
            self.records.insert(key.to_vec(), incoming);
            self.gap -= allocation;
        } else {
            assert_eq!(page.as_bytes(), before);
        }
    }

    fn remove_or_reference(&mut self, page: &mut Page, key: &[u8], reference: Option<bool>) {
        let expected = self
            .key_error(key)
            .map_or_else(|| Ok(self.records.contains_key(key)), Err);
        let before = page.as_bytes().to_vec();
        let actual = match reference {
            Some(value) => page.set_referenced(key, value),
            None => page.remove(key),
        };
        assert_eq!(actual, expected);
        if actual == Ok(true) {
            if let Some(value) = reference {
                self.records.get_mut(key).unwrap().referenced = value;
            } else {
                self.records.remove(key);
                self.gap += 8;
            }
        } else {
            assert_eq!(page.as_bytes(), before);
        }
    }

    fn compact(&mut self, page: &mut Page) {
        let packed_gap = self.capacity - self.packed_bytes();
        assert_eq!(page.compact(), packed_gap - self.gap);
        self.gap = packed_gap;
    }

    fn check(&self, page: &Page) {
        let bytes = page.as_bytes().to_vec();
        assert_eq!(page.len(), self.records.len());
        assert_eq!(page.is_empty(), self.records.is_empty());
        assert_eq!(page.capacity(), self.capacity);
        assert_eq!(page.free_bytes(), self.gap);
        assert_eq!(page.fences(), self.fences);
        let expected: Vec<_> = self
            .records
            .iter()
            .map(|(key, record)| record.borrow(key))
            .collect();
        let actual: Vec<_> = page.iter().collect();
        assert_eq!(actual, expected);
        for (key, record) in &self.records {
            assert_eq!(page.get(key), Ok(Some(record.borrow(key))));
        }
        for key in [b"".as_slice(), b"\0", b"a", b"ab", b"m", b"z", b"\xff\xff"] {
            let expected = self.key_error(key).map_or_else(
                || Ok(self.records.get(key).map(|record| record.borrow(key))),
                Err,
            );
            assert_eq!(page.get(key), expected);
        }
        for (start, end) in [
            (None, None),
            (Some(b"".as_slice()), Some(b"".as_slice())),
            (None, Some(b"ab".as_slice())),
            (Some(b"ab".as_slice()), None),
            (Some(b"a".as_slice()), Some(b"z".as_slice())),
            (Some(b"z".as_slice()), Some(b"a".as_slice())),
        ] {
            if start.zip(end).is_some_and(|(start, end)| start > end) {
                assert_eq!(page.range(start, end).err(), Some(PageError::InvalidRange));
            } else {
                let expected: Vec<_> = expected
                    .iter()
                    .copied()
                    .filter(|record| {
                        start.is_none_or(|start| record.key >= start)
                            && end.is_none_or(|end| record.key < end)
                    })
                    .collect();
                let iter = page.range(start, end).unwrap();
                assert_eq!(iter.len(), expected.len());
                let actual: Vec<_> = iter.collect();
                assert_eq!(actual, expected);
            }
        }

        // Independent raw-byte validation of structural slots, physical values,
        // previews, disjoint payloads, and holes above the modeled boundary.
        let count = self.records.len() + if self.fences.is_some() { 2 } else { 0 };
        let flags = 0x2000 | if self.fences.is_some() { 0x8000 } else { 0 };
        assert_eq!(read_u16(&bytes, 0), flags | count as u16);
        assert_eq!(usize::from(read_u16(&bytes, 2)), self.capacity);
        assert_eq!(usize::from(read_u16(&bytes, 4)), self.gap);
        assert_eq!(&bytes[6..8], &[0; 2]);
        assert_eq!(&bytes[8..32], &[0xa5; 24]);
        let slots_end = 32 + 8 * count;
        let boundary = slots_end + self.gap;
        assert!(boundary <= self.capacity);
        let mut occupied = vec![false; self.capacity];
        occupied[..slots_end].fill(true);
        let mut check_slot = |index: usize, key: &[u8], record: &ModelRecord| {
            let slot = 32 + index * 8;
            let offset = usize::from(read_u16(&bytes, slot));
            let key_word = read_u16(&bytes, slot + 2);
            let value_word = read_u16(&bytes, slot + 4);
            assert_eq!(key_word >> 14, record.kind);
            assert_eq!(usize::from(key_word & 0x3fff), key.len());
            assert_eq!(value_word >> 15, u16::from(record.referenced));
            assert_eq!(usize::from(value_word & 0x7fff), record.stored.len());
            if record.kind == 3 {
                assert!(record.stored.is_empty());
            }
            let end = offset + key.len() + record.stored.len();
            assert!(offset >= boundary && end <= self.capacity);
            assert_eq!(&bytes[offset..offset + key.len()], key);
            assert_eq!(&bytes[offset + key.len()..end], record.stored);
            assert_eq!(
                &bytes[slot + 6..slot + 8],
                &[
                    key.first().copied().unwrap_or(0),
                    key.get(1).copied().unwrap_or(0)
                ]
            );
            assert!(occupied[offset..end].iter().all(|&used| !used));
            occupied[offset..end].fill(true);
        };
        if let Some(fences) = self.fences {
            for (index, fence) in [fences.lower, fences.upper].into_iter().enumerate() {
                match fence {
                    Fence::Key(key) => {
                        check_slot(index, key, &ModelRecord::new(RecordState::Insert(b"")))
                    }
                    Fence::Unbounded => {
                        let slot = 32 + index * 8;
                        assert_eq!(read_u16(&bytes, slot), u16::MAX - 1 + index as u16);
                        assert_eq!(&bytes[slot + 2..slot + 8], &[0; 6]);
                    }
                }
            }
        }
        for (index, (key, record)) in self.records.iter().enumerate() {
            assert!(!key.is_empty());
            assert_eq!(self.key_error(key), None);
            check_slot(
                index + if self.fences.is_some() { 2 } else { 0 },
                key,
                record,
            );
        }
        assert!(occupied[slots_end..boundary].iter().all(|&used| !used));
        let holes = occupied[boundary..].iter().filter(|&&used| !used).count();
        assert_eq!(holes, self.capacity - self.packed_bytes() - self.gap);
        assert_eq!(page.reclaimable_bytes(), holes);
        assert_eq!(
            page.as_bytes(),
            bytes,
            "all reads must preserve the page image"
        );
    }
}

#[test]
fn mixed_mutations_match_physical_and_logical_models() {
    let profiles = [
        None,
        Some(Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        }),
        Some(Fences {
            lower: Fence::Key(b""),
            upper: Fence::Unbounded,
        }),
        Some(Fences {
            lower: Fence::Unbounded,
            upper: Fence::Key(b""),
        }),
        Some(Fences {
            lower: Fence::Key(b"a"),
            upper: Fence::Unbounded,
        }),
        Some(Fences {
            lower: Fence::Unbounded,
            upper: Fence::Key(b"m"),
        }),
        Some(Fences {
            lower: Fence::Key(b"a"),
            upper: Fence::Key(b"z"),
        }),
    ];
    let mut keys: Vec<_> = (0..16).map(|byte| vec![byte]).collect();
    keys.extend([
        b"".to_vec(),
        b"a".to_vec(),
        b"ab".to_vec(),
        b"abc".to_vec(),
        b"abd".to_vec(),
        b"b".to_vec(),
        b"m".to_vec(),
        b"y".to_vec(),
        b"z".to_vec(),
        b"\xff".to_vec(),
    ]);
    for capacity in [64, 128, 256, 448, 512, 832, 1024, 1600, 2048, 4032, 4096] {
        for fences in profiles {
            for seed in [1_u64, 0xfeed_beef] {
                let mut storage = vec![0xcc; capacity + 1];
                let page = match fences {
                    None => Page::init(&mut storage[1..]).unwrap(),
                    Some(fences) => Page::init_with_fences(&mut storage[1..], fences).unwrap(),
                };
                page.header
                    .count_and_flags
                    .set(page.header.count_and_flags.get() | 0x2000);
                page.as_mut_bytes()[8..32].fill(0xa5);
                let mut model = MutationModel::new(capacity, fences);
                model.check(page);
                let mut random = seed;
                let mut next = || {
                    random = random
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    (random >> 32) as usize
                };
                for step in 0..384 {
                    let key = &keys[next() % keys.len()];
                    let length = [0, 1, 2, 3, 7, 23, 31, capacity / 4, capacity][next() % 9];
                    let value: Vec<_> = (0..length).map(|index| (index ^ step) as u8).collect();
                    let state = [
                        RecordState::Insert(&value),
                        RecordState::Delete,
                        RecordState::Cache(&value),
                        RecordState::Phantom,
                    ][next() % 4];
                    match next() % 10 {
                        0..=2 => model.write(page, key, state, true),
                        3..=5 => model.write(page, key, state, false),
                        6 => model.remove_or_reference(page, key, Some(next().is_multiple_of(2))),
                        7 => model.remove_or_reference(page, key, None),
                        8 => model.compact(page),
                        _ => {} // check() includes point and bounded reads.
                    }
                    model.check(page);
                }
                model.compact(page);
                model.check(page);
                let remaining: Vec<_> = model.records.keys().cloned().collect();
                for key in remaining {
                    model.remove_or_reference(page, &key, None);
                    model.check(page);
                }
                model.compact(page);
                model.check(page);
                assert_eq!(storage[0], 0xcc);
            }
        }
    }
}

#[test]
fn fenced_compaction_preserves_counts_above_255_and_mixed_states() {
    let fences = Fences {
        lower: Fence::Key(b""),
        upper: Fence::Unbounded,
    };
    let mut storage = [0; 4096];
    let page = Page::init_with_fences(&mut storage, fences).unwrap();
    page.header
        .count_and_flags
        .set(page.header.count_and_flags.get() | 0x2000);
    page.as_mut_bytes()[8..32].fill(0xa5);
    let mut model = MutationModel::new(4096, Some(fences));
    for index in 0_u16..280 {
        let key = index.to_be_bytes();
        let state = [
            RecordState::Insert(b"v"),
            RecordState::Delete,
            RecordState::Cache(b"v"),
            RecordState::Phantom,
        ][usize::from(index) % 4];
        model.write(page, &key, state, true);
        model.remove_or_reference(page, &key, Some(true));
        model.check(page);
    }
    assert_eq!(page.len(), 280);
    for index in 0_u16..280 {
        if index.is_multiple_of(20) {
            model.remove_or_reference(page, &index.to_be_bytes(), None);
        } else if index.is_multiple_of(4) {
            model.write(page, &index.to_be_bytes(), RecordState::Delete, false);
        }
        model.check(page);
    }
    model.compact(page);
    model.check(page);
    assert!(page.len() > 255);
}
