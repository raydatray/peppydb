use super::*;

#[test]
fn initialize_all_capacities_on_unaligned_buffers() {
    for capacity in (64..=4096).step_by(64) {
        // Offset by one byte so the header's u64 fields are misaligned.
        let mut storage = vec![0xa5; capacity + 1];
        let page = Page::init(&mut storage[1..]).unwrap();
        assert_eq!(page.len(), 0);
        assert!(page.is_empty());
        assert_eq!(page.capacity(), capacity);
        assert_eq!(page.free_bytes(), capacity - HEADER_BYTES);
        assert_eq!(&page.as_bytes()[0..2], &[0, 0]);
        assert_eq!(&page.as_bytes()[2..4], &(capacity as u16).to_le_bytes());
        assert_eq!(
            &page.as_bytes()[4..6],
            &((capacity - 32) as u16).to_le_bytes()
        );
        assert_eq!(&page.as_bytes()[6..8], &[0, 0]);
        assert_eq!(&page.as_bytes()[8..16], &[0xff; 8]);
        assert!(page.as_bytes()[16..].iter().all(|&byte| byte == 0));
        assert_eq!(storage[0], 0xa5);
    }
}

#[test]
fn invalid_initialization_preserves_bytes() {
    for capacity in [0, 1, 16, 32, 63, 65, 96, 1000, 4095, 4097, 4160, 8192] {
        let mut storage = vec![0xa5; capacity];
        assert_eq!(
            Page::init(&mut storage).err(),
            Some(PageError::InvalidCapacity)
        );
        assert_eq!(storage, vec![0xa5; capacity]);
    }
}

#[test]
fn errors_implement_std_error_with_messages() {
    let cases = [
        (
            PageError::InvalidCapacity,
            "page capacity must be a multiple of 64 from 64 through 4096 bytes",
        ),
        (PageError::EmptyKey, "key must not be empty"),
        (
            PageError::RecordTooLarge,
            "record does not fit in an empty page",
        ),
        (PageError::DuplicateKey, "key already exists in the page"),
        (
            PageError::PageFull,
            "page has insufficient free space for the record",
        ),
    ];
    for (error, message) in cases {
        let error: &dyn std::error::Error = &error;
        assert_eq!(error.to_string(), message);
    }
}
