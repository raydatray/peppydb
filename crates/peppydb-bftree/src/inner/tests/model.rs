//! An independent logical model of one first child and sorted, owned
//! separator/right-child pairs, with physical sizes from the documented rule.

use super::*;

const HEADER: usize = 16;

fn entry_bytes(key: &[u8]) -> usize {
    8 + key.len().saturating_sub(4) + 8
}

#[derive(Clone, Debug)]
struct Model {
    capacity: usize,
    first: ChildRef,
    entries: Vec<(Vec<u8>, ChildRef)>,
}

impl Model {
    fn new(capacity: usize, first: ChildRef) -> Self {
        Self {
            capacity,
            first,
            entries: Vec::new(),
        }
    }

    fn used(&self) -> usize {
        HEADER
            + self
                .entries
                .iter()
                .map(|(key, _)| entry_bytes(key))
                .sum::<usize>()
    }

    fn child(&self, index: usize) -> Option<ChildRef> {
        match index {
            0 => Some(self.first),
            _ => self.entries.get(index - 1).map(|&(_, child)| child),
        }
    }

    fn route(&self, key: &[u8]) -> Route {
        let index = self
            .entries
            .iter()
            .filter(|(separator, _)| separator.as_slice() <= key)
            .count();
        Route {
            index,
            child: self.child(index).unwrap(),
        }
    }

    fn check(&self, key: &[u8], child: ChildRef) -> Result<usize, InnerError> {
        if key.is_empty() {
            return Err(InnerError::EmptyKey);
        }
        if child.is_leaf() != self.first.is_leaf() {
            return Err(InnerError::ChildKindMismatch);
        }
        if HEADER + entry_bytes(key) > self.capacity {
            return Err(InnerError::SeparatorTooLarge);
        }
        let position = self
            .entries
            .partition_point(|(stored, _)| stored.as_slice() < key);
        if self
            .entries
            .get(position)
            .is_some_and(|(stored, _)| stored == key)
        {
            return Err(InnerError::DuplicateSeparator);
        }
        Ok(position)
    }

    fn insert(&mut self, key: &[u8], child: ChildRef) -> Result<(), InnerError> {
        let position = self.check(key, child)?;
        if self.used() + entry_bytes(key) > self.capacity {
            return Err(InnerError::NodeFull);
        }
        self.entries.insert(position, (key.to_vec(), child));
        Ok(())
    }

    fn replace(&mut self, index: usize, child: ChildRef) -> Result<ChildRef, InnerError> {
        let old = self.child(index).ok_or(InnerError::InvalidChildIndex)?;
        if child.is_leaf() != self.first.is_leaf() {
            return Err(InnerError::ChildKindMismatch);
        }
        match index {
            0 => self.first = child,
            _ => self.entries[index - 1].1 = child,
        }
        Ok(old)
    }

    /// The virtual sequence with the pending entry, without changing `self`.
    fn with_pending(&self, key: &[u8], child: ChildRef) -> Result<Model, InnerError> {
        let position = self.check(key, child)?;
        let mut merged = self.clone();
        merged.entries.insert(position, (key.to_vec(), child));
        Ok(merged)
    }

    /// Exhaustively score every promotion and rebuild both sides.
    fn split(
        &self,
        right_capacity: usize,
        key: &[u8],
        child: ChildRef,
    ) -> Result<(Vec<u8>, Model, Model), InnerError> {
        if !(64..=4096).contains(&right_capacity) || !right_capacity.is_multiple_of(64) {
            return Err(InnerError::InvalidCapacity);
        }
        if right_capacity != self.capacity {
            return Err(InnerError::CapacityMismatch);
        }
        let merged = self.with_pending(key, child)?;
        let count = merged.entries.len();

        let side = |first, entries: &[(Vec<u8>, ChildRef)]| Model {
            capacity: self.capacity,
            first,
            entries: entries.to_vec(),
        };
        let mut best: Option<((usize, usize, usize), usize)> = None;
        for promoted in 1..count.saturating_sub(1) {
            let left = side(self.first, &merged.entries[..promoted]);
            let right = side(merged.entries[promoted].1, &merged.entries[promoted + 1..]);
            let (left, right) = (left.used(), right.used());
            if left > self.capacity || right > self.capacity {
                continue;
            }
            let score = (left.max(right), left.abs_diff(right), promoted);
            if best.is_none_or(|(best, _)| score < best) {
                best = Some((score, promoted));
            }
        }

        let (_, promoted) = best.ok_or(InnerError::CannotSplit)?;
        let (separator, right_first) = merged.entries[promoted].clone();
        Ok((
            separator,
            side(self.first, &merged.entries[..promoted]),
            side(right_first, &merged.entries[promoted + 1..]),
        ))
    }
}

/// Decode the node's raw bytes and compare them with the model, including
/// zero padding, tail placement, non-overlap, and exact free space.
fn validate(bytes: &[u8], model: &Model) {
    let capacity = model.capacity;
    let count = model.entries.len();
    let free = capacity - model.used();
    assert_eq!(bytes.len(), capacity);
    assert_eq!(usize::from(read_u16(bytes, 0)), count);
    assert_eq!(usize::from(read_u16(bytes, 2)), capacity);
    assert_eq!(usize::from(read_u16(bytes, 4)), free);
    assert_eq!(read_u16(bytes, 6), u16::from(!model.first.is_leaf()));
    assert_eq!(read_u64(bytes, 8), model.first.raw());

    let payload_start = HEADER + 8 * count + free;
    let mut payloads = Vec::new();
    for (index, (key, child)) in model.entries.iter().enumerate() {
        let slot = HEADER + 8 * index;
        let offset = usize::from(read_u16(bytes, slot));
        assert_eq!(usize::from(read_u16(bytes, slot + 2)), key.len());

        let inline = key.len().min(4);
        assert_eq!(bytes[slot + 4..slot + 4 + inline], key[..inline]);
        assert!(
            bytes[slot + 4 + inline..slot + 8]
                .iter()
                .all(|&byte| byte == 0)
        );

        let tail = &key[inline..];
        assert_eq!(&bytes[offset..offset + tail.len()], tail);
        assert_eq!(read_u64(bytes, offset + tail.len()), child.raw());
        payloads.push(offset..offset + tail.len() + 8);
    }

    payloads.sort_by_key(|payload| payload.start);
    assert!(
        payloads
            .first()
            .is_none_or(|payload| payload.start == payload_start)
    );
    assert!(payloads.windows(2).all(|pair| pair[0].end == pair[1].start));
    assert!(
        payloads
            .last()
            .is_none_or(|payload| payload.end == capacity)
    );
}

fn compare(node: &InnerNode, model: &Model) {
    validate(node.as_bytes(), model);
    assert_eq!(node.len(), model.entries.len());
    assert_eq!(node.free_bytes(), model.capacity - model.used());
    let stored: Vec<_> = node
        .iter()
        .map(|entry| (entry.key.to_vec(), entry.right_child))
        .collect();
    assert_eq!(stored, model.entries);
    for index in 0..node.child_count() + 1 {
        assert_eq!(node.child_at(index), model.child(index));
    }
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }

    /// Keys over a tiny alphabet with lengths around the four inline bytes,
    /// plus long keys sharing a prefix, so collisions are common.
    fn key(&mut self, capacity: usize) -> Vec<u8> {
        let length = match self.below(8) {
            0 => 0,
            1..=5 => 1 + self.below(6),
            6 => 4 + self.below(12),
            _ => 1 + self.below(capacity / 2),
        };
        let mut key: Vec<u8> = (0..length)
            .map(|_| [0, 1, b'a', 0xff][self.below(4)])
            .collect();
        if length > 8 && self.below(2) == 0 {
            key[..8].copy_from_slice(b"collide/");
        }
        key
    }

    fn child(&mut self, leaves: bool) -> ChildRef {
        let raw = self.below(1 << 20) as u64;
        // Occasionally use the wrong kind.
        if (self.below(16) == 0) == leaves {
            inner(raw)
        } else {
            leaf(raw)
        }
    }
}

fn queries(model: &Model, rng: &mut Rng) -> Vec<Vec<u8>> {
    let mut queries = vec![b"".to_vec(), vec![0xff; 70]];
    for (key, _) in &model.entries {
        queries.push(key.clone());
        queries.push([key.as_slice(), &[0]].concat());
        queries.push(key[..key.len() - 1].to_vec());
    }
    queries.extend((0..8).map(|_| rng.key(model.capacity)));
    queries
}

/// Compose a one-separator parent over the split outputs and check every
/// sampled query against the unsplit virtual sequence.
fn check_composed_routing(merged: &Model, split: &InnerSplit<'_>, left: &InnerNode, rng: &mut Rng) {
    for query in queries(merged, rng) {
        let side = if query < split.separator {
            left
        } else {
            &*split.right
        };
        assert_eq!(
            side.route(&query).child,
            merged.route(&query).child,
            "{query:?}"
        );
    }
}

/// Run one operation sequence; return the number of successful splits.
fn run(capacity: usize, leaves: bool, seed: u64, steps: usize) -> usize {
    let mut rng = Rng(seed);
    let first = if leaves { leaf(0) } else { inner(0) };
    let mut storage = vec![0; capacity];
    let mut model = Model::new(capacity, first);
    InnerNode::init(&mut storage, first).unwrap();
    let mut splits = 0;

    for _ in 0..steps {
        let node = InnerNode::mut_from_bytes(&mut storage).unwrap();
        match rng.below(10) {
            0..=5 => {
                let key = rng.key(capacity);
                let child = rng.child(leaves);
                let before = node.as_bytes().to_vec();
                let result = node.try_insert(&key, child);
                assert_eq!(result, model.insert(&key, child), "{key:?}");
                if result.is_err() {
                    assert_eq!(node.as_bytes(), before);
                }
                if result == Err(InnerError::NodeFull) && rng.below(2) == 0 {
                    splits += usize::from(split(node, &mut model, &key, child, &mut rng));
                }
            }
            6..=7 => {
                let index = rng.below(model.entries.len() + 3);
                let child = rng.child(leaves);
                let before = node.as_bytes().to_vec();
                let result = node.replace_child(index, child);
                assert_eq!(result, model.replace(index, child));
                if result.is_err() {
                    assert_eq!(node.as_bytes(), before);
                }
            }
            _ => {
                for query in queries(&model, &mut rng) {
                    assert_eq!(node.route(&query), model.route(&query), "{query:?}");
                }
            }
        }
        compare(InnerNode::ref_from_bytes(&storage).unwrap(), &model);
    }
    splits
}

/// Split, compare both outputs with the oracle, and continue with one side.
/// Return whether the split succeeded.
fn split(
    node: &mut InnerNode,
    model: &mut Model,
    key: &[u8],
    child: ChildRef,
    rng: &mut Rng,
) -> bool {
    let mut right_storage = vec![0xa5; model.capacity];
    let before = node.as_bytes().to_vec();
    let expected = model.split(model.capacity, key, child);
    let merged = model.with_pending(key, child);

    match node.split_with_insert(&mut right_storage, key, child) {
        Err(error) => {
            assert_eq!(Err(error), expected.map(|_| ()));
            assert_eq!(node.as_bytes(), before);
            assert_eq!(right_storage, vec![0xa5; model.capacity]);
            false
        }
        Ok(split) => {
            let (separator, left, right) = expected.unwrap();
            assert_eq!(split.separator, separator);
            compare(split.right, &right);
            compare(node, &left);
            check_composed_routing(&merged.unwrap(), &split, node, rng);
            // Children are conserved: m separators had m + 1 children.
            assert_eq!(
                node.child_count() + split.right.child_count(),
                model.entries.len() + 2
            );

            if rng.below(2) == 0 {
                *model = left;
            } else {
                node.as_mut_bytes().copy_from_slice(split.right.as_bytes());
                *model = right;
            }
            true
        }
    }
}

#[test]
fn operation_sequences_match_the_model() {
    for (capacity, steps) in [(64, 400), (128, 600), (512, 800), (4096, 1500)] {
        let splits: usize = [(1, true), (2, false), (3, true)]
            .into_iter()
            .map(|(seed, leaves)| {
                let seed = 0x9e37_79b9 ^ (seed * 0x1_0000_0001) ^ capacity as u64;
                run(capacity, leaves, seed, steps)
            })
            .sum();
        assert!(splits >= 20, "only {splits} splits at capacity {capacity}");
    }
}

#[test]
fn separators_fill_a_4096_byte_node_exactly() {
    let mut storage = [0; 4096];
    let node = InnerNode::init(&mut storage, leaf(0)).unwrap();
    let mut model = Model::new(4096, leaf(0));
    // 255 short separators use 255 * 16 = 4080 bytes.
    for raw in 0..255u8 {
        let key = [1, raw];
        node.try_insert(&key, leaf(u64::from(raw) + 1)).unwrap();
        model.insert(&key, leaf(u64::from(raw) + 1)).unwrap();
    }
    compare(node, &model);
    assert_eq!(node.free_bytes(), 0);
    assert_eq!(
        node.try_insert(b"\x02", leaf(999)),
        Err(InnerError::NodeFull)
    );
}
