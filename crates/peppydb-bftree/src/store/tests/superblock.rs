use super::*;

#[test]
fn superblocks_round_trip_through_a_page() {
    for page_size in [64, 256, 4096] {
        let superblock = Superblock {
            page_size,
            inner_capacity: 4096,
            max_key_len: 64,
            max_value_len: 1024,
        };
        let mut page = vec![0xff; page_size];
        superblock.encode(&mut page).unwrap();

        assert_eq!(&page[..8], &MAGIC);
        assert!(
            page[ENCODED_BYTES..].iter().all(|&byte| byte == 0),
            "padding is zeroed"
        );
        assert_eq!(Superblock::decode(&page), Ok(superblock));
    }
}

#[test]
fn encode_requires_a_page_sized_buffer() {
    let superblock = superblock(128);
    let mut short = vec![0; 127];
    assert_eq!(
        superblock.encode(&mut short),
        Err(StoreError::BufferLength {
            expected: 128,
            actual: 127,
        })
    );
}

#[test]
fn invalid_configurations_are_rejected_before_encoding() {
    let mut page = vec![0; 100];
    let odd = Superblock {
        page_size: 100,
        ..superblock(128)
    };
    assert!(matches!(
        odd.encode(&mut page),
        Err(StoreError::InvalidSuperblock(_))
    ));

    let huge = Superblock {
        max_value_len: usize::MAX,
        ..superblock(128)
    };
    assert!(matches!(
        huge.validate(),
        Err(StoreError::InvalidSuperblock(_))
    ));
}

#[test]
fn decode_rejects_bad_magic_version_and_page_size() {
    let mut page = vec![0; 128];
    superblock(128).encode(&mut page).unwrap();

    let mut bad_magic = page.clone();
    bad_magic[0] = b'X';
    assert_eq!(
        Superblock::decode(&bad_magic),
        Err(StoreError::InvalidSuperblock("bad magic"))
    );

    let mut bad_version = page.clone();
    bad_version[8..12].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(
        Superblock::decode(&bad_version),
        Err(StoreError::InvalidSuperblock("unsupported version"))
    );

    let mut bad_page_size = page.clone();
    bad_page_size[12..16].copy_from_slice(&100u32.to_le_bytes());
    assert!(matches!(
        Superblock::decode(&bad_page_size),
        Err(StoreError::InvalidSuperblock(_))
    ));

    assert_eq!(
        Superblock::decode(&page[..ENCODED_BYTES - 1]),
        Err(StoreError::InvalidSuperblock("too short"))
    );
    assert_eq!(
        Superblock::decode(&page[..ENCODED_BYTES]),
        Ok(superblock(128))
    );
}

#[test]
fn mem_store_rejects_an_invalid_superblock() {
    let bad = Superblock {
        page_size: 0,
        ..superblock(128)
    };
    assert!(matches!(
        MemStore::new(bad).err(),
        Some(StoreError::InvalidSuperblock(_))
    ));
}
