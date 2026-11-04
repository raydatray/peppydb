use super::*;

/// Encode a node from the documented format, independent of production code.
/// Payloads are placed from the end of the buffer in entry order.
fn fixture(capacity: usize, first: ChildRef, entries: &[(&[u8], ChildRef)]) -> Vec<u8> {
    let mut bytes = vec![0; capacity];
    let mut end = capacity;
    for (index, &(key, child)) in entries.iter().enumerate() {
        let tail = key.get(4..).unwrap_or_default();
        end -= tail.len() + 8;
        bytes[end..end + tail.len()].copy_from_slice(tail);
        bytes[end + tail.len()..end + tail.len() + 8].copy_from_slice(&child.raw().to_le_bytes());

        let slot = 16 + 8 * index;
        bytes[slot..slot + 2].copy_from_slice(&(end as u16).to_le_bytes());
        bytes[slot + 2..slot + 4].copy_from_slice(&(key.len() as u16).to_le_bytes());
        let inline = key.len().min(4);
        bytes[slot + 4..slot + 4 + inline].copy_from_slice(&key[..inline]);
    }

    let free = end - 16 - 8 * entries.len();
    bytes[0..2].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    bytes[2..4].copy_from_slice(&(capacity as u16).to_le_bytes());
    bytes[4..6].copy_from_slice(&(free as u16).to_le_bytes());
    bytes[6..8].copy_from_slice(&u16::from(!first.is_leaf()).to_le_bytes());
    bytes[8..16].copy_from_slice(&first.raw().to_le_bytes());
    bytes
}

fn view(bytes: &[u8]) -> &InnerNode {
    InnerNode::ref_from_bytes(bytes).unwrap()
}

/// Reference routing: the number of separators less than or equal to `key`.
fn model_route(separators: &[&[u8]], key: &[u8]) -> usize {
    separators
        .iter()
        .filter(|separator| **separator <= key)
        .count()
}

#[test]
fn route_partitions_queries_with_right_on_equality() {
    let bytes = fixture(128, leaf(10), &[(b"m", leaf(11)), (b"t", leaf(12))]);
    let node = view(&bytes);

    let cases: [(&[u8], usize); 9] = [
        (b"", 0),
        (b"a", 0),
        (b"l\xff", 0),
        (b"m", 1),
        (b"m\0", 1),
        (b"s", 1),
        (b"t", 2),
        (b"z", 2),
        (b"\xff\xff\xff\xff\xff", 2),
    ];
    for (key, index) in cases {
        let child = leaf(10 + index as u64);
        assert_eq!(node.route(key), Route { index, child }, "{key:?}");
    }
}

#[test]
fn node_without_separators_routes_to_its_first_child() {
    let bytes = fixture(64, inner(0), &[]);
    let node = view(&bytes);
    for key in [&b""[..], b"a", b"\xff\xff\xff\xff\xff\xff"] {
        assert_eq!(
            node.route(key),
            Route {
                index: 0,
                child: inner(0)
            }
        );
    }
    assert_eq!(node.iter().len(), 0);
}

#[test]
fn short_keys_and_four_byte_collisions_route_by_full_bytes() {
    let separators: [&[u8]; 10] = [
        b"\0", b"ab", b"abc", b"abc\0", b"abcd", b"abcd\0", b"abcdA", b"abcdAB", b"abce", b"b",
    ];
    let entries: Vec<_> = separators
        .iter()
        .zip(1..)
        .map(|(key, raw)| (*key, inner(raw)))
        .collect();
    let bytes = fixture(512, inner(0), &entries);
    let node = view(&bytes);

    let mut queries: Vec<Vec<u8>> = separators.iter().map(|key| key.to_vec()).collect();
    for key in separators {
        queries.push([key, b"\0"].concat());
        queries.push([key, b"\xff"].concat());
        queries.push(key[..key.len() - 1].to_vec());
    }
    queries.extend([b"".to_vec(), b"abcc\xff\xff".to_vec(), b"abcdAA".to_vec()]);

    for query in queries {
        let index = model_route(&separators, &query);
        assert_eq!(
            node.route(&query),
            Route {
                index,
                child: inner(index as u64)
            },
            "{query:?}"
        );
    }
}

#[test]
fn iteration_borrows_inline_and_tail_parts() {
    let entries: [(&[u8], ChildRef); 3] = [
        (b"ab", leaf(1)),
        (b"abcd", leaf(u64::MAX - 1)),
        (b"abcdefgh", leaf(0)),
    ];
    let bytes = fixture(128, leaf(9), &entries);
    let node = view(&bytes);
    let directory = bytes[16..40].as_ptr_range();
    let payloads = bytes[40..].as_ptr_range();

    let iter = node.iter();
    assert_eq!(iter.len(), 3);
    for (separator, (key, child)) in iter.zip(entries) {
        assert_eq!(separator.key.to_vec(), key);
        assert_eq!(separator.key.prefix(), &key[..key.len().min(4)]);
        assert_eq!(separator.key.suffix(), key.get(4..).unwrap_or_default());
        assert_eq!(separator.right_child, child);
        assert!(directory.contains(&separator.key.prefix().as_ptr()));
        if !separator.key.suffix().is_empty() {
            assert!(payloads.contains(&separator.key.suffix().as_ptr()));
        }
    }

    let mut iter = node.into_iter();
    iter.by_ref().for_each(drop);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next(), None);
}

#[test]
fn child_access_covers_every_index_and_stops_after_the_last() {
    let bytes = fixture(128, inner(100), &[(b"k1", inner(101)), (b"k2", inner(102))]);
    let node = view(&bytes);
    assert_eq!(node.first_child(), inner(100));
    assert_eq!(node.child_count(), 3);
    for index in 0..3 {
        assert_eq!(node.child_at(index), Some(inner(100 + index as u64)));
    }
    assert_eq!(node.child_at(3), None);
    assert_eq!(node.child_at(usize::MAX), None);
}

/// Deterministic xorshift generator, so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// Exercise every `&self` method; the bytes may be arbitrary.
fn read_everything(node: &InnerNode, queries: &[&[u8]]) {
    let _ = (node.is_empty(), node.capacity(), node.free_bytes());
    let _ = node.first_child();
    for index in 0..node.child_count() + 2 {
        let _ = node.child_at(index);
    }
    for query in queries {
        let route = node.route(query);
        assert!(route.index <= node.len());
    }
    let iter = node.iter();
    assert_eq!(iter.len(), node.len());
    for separator in iter {
        let _ = (separator.key.to_vec(), separator.key.cmp_bytes(b"abcde"));
    }
}

#[test]
fn reads_are_total_over_random_bytes() {
    let queries: [&[u8]; 5] = [b"", b"a", b"abcd", b"abcde\xff", &[0xff; 64]];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let lengths: Vec<usize> = [16, 17, 23, 24, 63, 64, 100, 128, 4096].into();
    for &length in &lengths {
        for _ in 0..200 {
            let mut bytes: Vec<u8> = (0..length).map(|_| rng.next() as u8).collect();
            // Bias some slot fields toward the buffer end to stress clamping.
            if length > 24 && rng.below(2) == 0 {
                let slot = 16 + 8 * rng.below((length - 16) / 8);
                let offset = (length - rng.below(16)) as u16;
                bytes[slot..slot + 2].copy_from_slice(&offset.to_le_bytes());
            }
            read_everything(view(&bytes), &queries);
        }
    }
}

#[test]
fn reads_are_total_over_adversarial_bytes() {
    let queries: [&[u8]; 3] = [b"", b"\xff\xff\xff\xff\xff", b"\0"];
    for length in [16, 64, 4096] {
        for fill in [0x00, 0x80, 0xff] {
            read_everything(view(&vec![fill; length]), &queries);
        }

        // A count claiming every slot, each pointing at the final byte with a
        // maximal key length.
        let mut bytes = vec![0; length];
        bytes[0..2].copy_from_slice(&u16::MAX.to_le_bytes());
        for slot in (16..length - 7).step_by(8) {
            bytes[slot..slot + 2].copy_from_slice(&((length - 1) as u16).to_le_bytes());
            bytes[slot + 2..slot + 4].copy_from_slice(&u16::MAX.to_le_bytes());
        }
        read_everything(view(&bytes), &queries);
    }
}

#[test]
fn reserved_and_unknown_stored_values_decode_without_panicking() {
    let mut bytes = fixture(64, leaf(1), &[]);
    bytes[6..8].copy_from_slice(&7u16.to_le_bytes());
    bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(view(&bytes).first_child(), inner(u64::MAX - 1));
}
