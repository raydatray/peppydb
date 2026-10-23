use super::*;

#[test]
fn initialize_all_capacities_on_unaligned_buffers() {
    for capacity in (64..=4096).step_by(64) {
        for (first, kind) in [(leaf(0x0102_0304_0506_0708), 0), (inner(u64::MAX - 1), 1)] {
            // Offset by one byte so the header's u64 field is misaligned.
            let mut storage = vec![0xa5; capacity + 1];
            let node = InnerNode::init(&mut storage[1..], first).unwrap();
            assert_eq!(node.len(), 0);
            assert!(node.is_empty());
            assert_eq!(node.child_count(), 1);
            assert_eq!(node.capacity(), capacity);
            assert_eq!(node.free_bytes(), capacity - 16);

            let bytes = node.as_bytes();
            assert_eq!(read_u16(bytes, 0), 0);
            assert_eq!(usize::from(read_u16(bytes, 2)), capacity);
            assert_eq!(usize::from(read_u16(bytes, 4)), capacity - 16);
            assert_eq!(read_u16(bytes, 6), kind);
            assert_eq!(read_u64(bytes, 8), first.raw());
            assert!(bytes[16..].iter().all(|&byte| byte == 0));
            assert_eq!(storage[0], 0xa5);
        }
    }
}

#[test]
fn golden_header_bytes() {
    let mut storage = [0xa5; 64];
    InnerNode::init(&mut storage, inner(0x1122_3344_5566_7788)).unwrap();
    assert_eq!(
        storage[..16],
        [
            0, 0, // count
            64, 0, // capacity
            48, 0, // remaining_size
            1, 0, // children_kind: inner
            0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // first_child
        ]
    );
}

#[test]
fn invalid_initialization_preserves_bytes() {
    for capacity in [0, 1, 15, 16, 32, 63, 65, 96, 1000, 4095, 4097, 4160, 8192] {
        let mut storage = vec![0xa5; capacity];
        assert_eq!(
            InnerNode::init(&mut storage, leaf(1)).err(),
            Some(InnerError::InvalidCapacity)
        );
        assert_eq!(storage, vec![0xa5; capacity]);
    }
}

#[test]
fn errors_implement_std_error_with_messages() {
    let cases = [
        (
            InnerError::InvalidCapacity,
            "node capacity must be a multiple of 64 from 64 through 4096 bytes",
        ),
        (InnerError::EmptyKey, "separator key must not be empty"),
        (
            InnerError::NodeFull,
            "node has insufficient free space for the separator",
        ),
        (
            InnerError::CannotSplit,
            "entries cannot form two nodes that fit and each keep a separator",
        ),
    ];
    for (error, message) in cases {
        let error: &dyn std::error::Error = &error;
        assert_eq!(error.to_string(), message);
    }
}

#[test]
fn counts_are_clamped_to_the_buffer() {
    let mut storage = [0xff; 64];
    let node = InnerNode::ref_from_bytes(&storage[..]).unwrap();
    // 48 data bytes hold at most six slots.
    assert_eq!(node.len(), 6);
    assert_eq!(node.child_count(), 7);

    storage[..2].copy_from_slice(&3u16.to_le_bytes());
    let node = InnerNode::ref_from_bytes(&storage[..]).unwrap();
    assert_eq!(node.len(), 3);
}
