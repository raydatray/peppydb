//! A set of leaves driven by random operations against a `BTreeMap` oracle.
//!
//! Each leaf is a fenced base page plus an optional mini-page whose block
//! comes from one shared circular buffer. Leaves are routed by a linear
//! search over their fences. Writes go through `buffer`; `NeedsMerge` merges
//! the leaf (splitting it when the merge does not fit) and the write retries.

use std::collections::BTreeMap;

use bf_tree_circular_buffer::CircularBuffer;
use bf_tree_page::{Fence, Fences, KeyRef, Page, RecordKind, RecordState};
use zerocopy::FromBytes;

use super::*;

struct Leaf {
    id: LeafId,
    base: Vec<u8>,
    mini: Option<Mini>,
}

impl Leaf {
    fn base(&self) -> &Page {
        Page::ref_from_bytes(&self.base).unwrap()
    }

    fn contains(&self, key: &[u8]) -> bool {
        let fences = self.base().fences().unwrap();
        let key = KeyRef::from(key);
        let above = match fences.lower {
            Fence::Unbounded => true,
            Fence::Key(lower) => key >= lower,
        };
        let below = match fences.upper {
            Fence::Unbounded => true,
            Fence::Key(upper) => key < upper,
        };
        above && below
    }
}

struct Model {
    capacity: usize,
    max_key: usize,
    max_value: usize,
    classes: SizeClasses,
    buffer: CircularBuffer,
    /// In key order; they tile the key space.
    leaves: Vec<Leaf>,
    oracle: BTreeMap<Vec<u8>, Vec<u8>>,
    next_id: u64,
    rng: u64,
}

impl Model {
    fn new(capacity: usize, max_key: usize, max_value: usize, seed: u64) -> Self {
        let classes = SizeClasses::derive(capacity, max_key, max_value).unwrap();
        let mut ladder = classes.as_slice().to_vec();
        ladder.push(capacity);
        ladder.dedup();
        let buffer = CircularBuffer::new(1 << 22, &ladder, 0).unwrap();
        let mut base = vec![0; capacity];
        let unbounded = Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        };
        Page::init_with_fences(&mut base, unbounded).unwrap();
        Self {
            capacity,
            max_key,
            max_value,
            classes,
            buffer,
            leaves: vec![Leaf {
                id: leaf(0),
                base,
                mini: None,
            }],
            oracle: BTreeMap::new(),
            next_id: 1,
            rng: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        }
    }

    fn next(&mut self, bound: usize) -> usize {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % bound as u64) as usize
    }

    /// Short keys over a small alphabet share prefixes; some have the maximum
    /// length.
    fn key(&mut self) -> Vec<u8> {
        let len = if self.next(10) == 0 {
            self.max_key
        } else {
            1 + self.next(self.max_key.min(4))
        };
        (0..len).map(|_| b'a' + self.next(6) as u8).collect()
    }

    /// Empty, maximum, and in-between values.
    fn value(&mut self) -> Vec<u8> {
        let len = match self.next(4) {
            0 => self.max_value,
            1 => 0,
            _ => self.next(self.max_value / 4 + 1),
        };
        vec![self.next(256) as u8; len]
    }

    fn route(&self, key: &[u8]) -> usize {
        self.leaves
            .iter()
            .position(|leaf| leaf.contains(key))
            .unwrap()
    }

    fn write(&mut self, key: &[u8], state: RecordState<'_>) {
        loop {
            let index = self.route(key);
            let leaf = &mut self.leaves[index];
            let before = leaf.base.clone();
            let outcome = buffer(
                &mut leaf.mini,
                &self.buffer,
                leaf.id,
                &self.classes,
                key,
                state,
            )
            .unwrap();
            assert_eq!(leaf.base, before, "a buffered write never changes the base");
            match outcome {
                WriteOutcome::Buffered => break,
                WriteOutcome::SkippedDirty => panic!("writes are dirty"),
                WriteOutcome::NeedsMerge => self.merge(index),
            }
        }
        match state {
            RecordState::Insert(value) => {
                self.oracle.insert(key.to_vec(), value.to_vec());
            }
            _ => {
                self.oracle.remove(key);
            }
        }
    }

    /// Merge leaf `index`'s mini-page into its base, splitting if needed,
    /// and free the mini-page's block.
    fn merge(&mut self, index: usize) {
        let leaf = &mut self.leaves[index];
        let mut leaf_mut = LeafMut {
            base: Page::mut_from_bytes(&mut leaf.base).unwrap(),
            mini: leaf.mini.as_deref(),
        };
        let mut right = None;
        if leaf_mut.merge_fits().unwrap() {
            leaf_mut.merge_in_place().unwrap();
        } else {
            let mut bytes = vec![0; self.capacity];
            leaf_mut.merge_split(&mut bytes).unwrap();
            right = Some(bytes);
        }
        if let Some(mini) = leaf.mini.take() {
            self.buffer.free(mini.block);
        }
        if let Some(base) = right {
            let id = leaf_id(&mut self.next_id);
            self.leaves.insert(
                index + 1,
                Leaf {
                    id,
                    base,
                    mini: None,
                },
            );
        }
    }

    /// Buffer a clean record that repeats the oracle. Fills never merge: a
    /// fill that finds the mini-page full at the largest class is dropped.
    fn cache_fill(&mut self, key: &[u8]) {
        let value = self.oracle.get(key).cloned();
        let state = match &value {
            Some(value) => RecordState::Cache(value),
            None => RecordState::Phantom,
        };
        let index = self.route(key);
        let leaf = &mut self.leaves[index];
        let before = leaf.base.clone();
        // On `NeedsMerge` the fill is dropped; nothing changed.
        buffer(
            &mut leaf.mini,
            &self.buffer,
            leaf.id,
            &self.classes,
            key,
            state,
        )
        .unwrap();
        assert_eq!(leaf.base, before);
    }

    fn lookup(&self, key: &[u8]) {
        let leaf = &self.leaves[self.route(key)];
        let found = match leaf.mini.as_ref().map(|mini| mini.lookup_marking(key)) {
            Some(Lookup::Found(value)) => Some(value.to_vec()),
            Some(Lookup::Absent) => None,
            Some(Lookup::Miss) | None => leaf
                .base()
                .get(key)
                .unwrap()
                .map(|record| record.state.value().unwrap().to_vec()),
        };
        assert_eq!(found.as_ref(), self.oracle.get(key), "lookup of {key:?}");
    }

    fn scan(&self, start: Option<&[u8]>, end: Option<&[u8]>) {
        let records: Vec<_> = self
            .leaves
            .iter()
            .flat_map(|leaf| {
                let view = LeafRef {
                    base: leaf.base(),
                    mini: leaf.mini.as_deref(),
                };
                view.range(start, end)
                    .unwrap()
                    .map(|record| (record.key.to_vec(), record.state.value().unwrap().to_vec()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let expected: Vec<_> = self
            .oracle
            .iter()
            .filter(|(key, _)| start.is_none_or(|s| key.as_slice() >= s))
            .filter(|(key, _)| end.is_none_or(|e| key.as_slice() < e))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        assert_eq!(records, expected, "scan of [{start:?}, {end:?})");
    }

    fn step(&mut self) {
        let key = self.key();
        match self.next(100) {
            0..40 => {
                let value = self.value();
                self.write(&key, RecordState::Insert(&value));
            }
            40..55 => self.write(&key, RecordState::Delete),
            55..70 => self.cache_fill(&key),
            70..88 => self.lookup(&key),
            88..95 => {
                let other = self.key();
                let (start, end) = if key <= other {
                    (key, other)
                } else {
                    (other, key)
                };
                self.scan(Some(&start), Some(&end));
            }
            _ => {
                let index = self.next(self.leaves.len());
                self.merge(index);
            }
        }
    }

    fn check(&self) {
        // Base pages tile the key space from unbounded to unbounded.
        let first = self.leaves.first().unwrap().base().fences().unwrap();
        let last = self.leaves.last().unwrap().base().fences().unwrap();
        assert_eq!(first.lower, Fence::Unbounded);
        assert_eq!(last.upper, Fence::Unbounded);
        for pair in self.leaves.windows(2) {
            let left = pair[0].base().fences().unwrap().upper;
            let right = pair[1].base().fences().unwrap().lower;
            assert_eq!(left, right, "adjacent leaves share a fence");
        }

        for leaf in &self.leaves {
            // Base: valid, fenced, no backing leaf, Insert records only.
            let base = Page::validate(&leaf.base).unwrap();
            assert!(base.fences().is_some());
            assert_eq!(base.backing_leaf(), None);
            assert!(
                base.iter()
                    .all(|record| record.state.kind() == RecordKind::Insert)
            );

            let Some(mini) = &leaf.mini else { continue };
            // Mini-page: valid, unfenced, names its leaf, a class capacity.
            let page = Page::validate(mini.block.as_ref()).unwrap();
            assert!(page.fences().is_none());
            assert_eq!(page.backing_leaf(), Some(leaf.id));
            assert!(self.classes.as_slice().contains(&page.capacity()));
            for record in page.iter() {
                let key = record.key.to_vec();
                // Every buffered key lies inside its leaf's fences.
                assert!(leaf.contains(&key));
                // Clean records repeat the oracle.
                match record.state {
                    RecordState::Cache(value) => {
                        assert_eq!(Some(&value.to_vec()), self.oracle.get(&key))
                    }
                    RecordState::Phantom => assert!(!self.oracle.contains_key(&key)),
                    _ => {}
                }
            }
        }
        self.scan(None, None);
    }

    /// Merge every mini-page; the buffer must then be empty.
    fn finish(mut self) {
        for index in (0..self.leaves.len()).rev() {
            self.merge(index);
        }
        self.check();
        assert_eq!(self.buffer.used(), 0);
    }
}

fn leaf_id(next: &mut u64) -> LeafId {
    let id = leaf(*next);
    *next += 1;
    id
}

#[test]
fn a_leaf_set_agrees_with_a_map() {
    for (capacity, max_key, max_value, steps) in [
        (4096, 64, 1024, 3000),
        (512, 16, 32, 4000),
        (128, 8, 0, 4000),
    ] {
        for seed in 0..3 {
            let mut model = Model::new(capacity, max_key, max_value, seed);
            for step in 0..steps {
                model.step();
                // Validating every page is slow; check densely early on.
                if step < 256 || step % 16 == 0 {
                    model.check();
                }
            }
            assert!(
                model.leaves.len() > 1,
                "({capacity}, {max_key}, {max_value}) never split"
            );
            model.finish();
        }
    }
}
