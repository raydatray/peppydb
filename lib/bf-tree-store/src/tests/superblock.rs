use super::*;

fn state(checkpoint_id: u64) -> State {
    State {
        checkpoint_id,
        applied_seq: checkpoint_id * 10,
        write_seq: checkpoint_id * 100,
    }
}

fn copy(superblock: Superblock, state: State) -> Vec<u8> {
    let mut block = vec![0xa5; COPY_BYTES];
    superblock.encode(&state, &mut block).unwrap();
    block
}

#[test]
fn copies_round_trip_with_their_state() {
    for page_size in [64, 256, 4096] {
        let superblock = Superblock {
            page_size,
            inner_capacity: 4096,
            max_key_len: 64,
            max_value_len: 1024,
        };
        let block = copy(superblock, state(7));
        assert_eq!(&block[..8], &MAGIC);
        assert_eq!(&block[8..12], &VERSION.to_le_bytes());
        assert!(
            block[ENCODED_BYTES..].iter().all(|&byte| byte == 0),
            "padding is zeroed"
        );
        assert_eq!(Superblock::decode(&block), Ok((superblock, state(7))));
        assert_eq!(
            Superblock::decode(&block[..ENCODED_BYTES]),
            Ok((superblock, state(7)))
        );
    }
}

#[test]
fn every_encoded_byte_is_checked() {
    let block = copy(superblock(128), state(3));
    for index in 0..ENCODED_BYTES {
        let mut damaged = block.clone();
        damaged[index] ^= 0x01;
        assert!(Superblock::decode(&damaged).is_err(), "byte {index}");
    }
    assert_eq!(
        Superblock::decode(&block[..ENCODED_BYTES - 1]),
        Err(StoreError::InvalidSuperblock("too short"))
    );
}

#[test]
fn decode_names_what_failed() {
    let block = copy(superblock(128), state(1));
    let mut bad_magic = block.clone();
    bad_magic[0] = b'X';
    assert_eq!(
        Superblock::decode(&bad_magic),
        Err(StoreError::InvalidSuperblock("bad magic"))
    );
    let mut version_1 = block.clone();
    version_1[8..12].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(
        Superblock::decode(&version_1),
        Err(StoreError::InvalidSuperblock("unsupported version"))
    );
    let mut flipped = block.clone();
    flipped[40] ^= 0x80;
    assert_eq!(
        Superblock::decode(&flipped),
        Err(StoreError::InvalidSuperblock("checksum mismatch"))
    );
}

#[test]
fn encode_needs_room_and_a_valid_configuration() {
    let mut short = vec![0; ENCODED_BYTES - 1];
    assert_eq!(
        superblock(128).encode(&State::default(), &mut short),
        Err(StoreError::BufferLength {
            expected: ENCODED_BYTES,
            actual: ENCODED_BYTES - 1,
        })
    );
    let mut block = vec![0; COPY_BYTES];
    let odd = Superblock {
        page_size: 100,
        ..superblock(128)
    };
    assert!(matches!(
        odd.encode(&State::default(), &mut block),
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
fn select_takes_the_newer_intact_copy() {
    let config = superblock(128);
    let (older, newer) = (copy(config, state(4)), copy(config, state(5)));
    assert_eq!(Superblock::select(&older, &newer), Ok((config, state(5))));
    assert_eq!(Superblock::select(&newer, &older), Ok((config, state(5))));
    // Both copies of a new store hold the same state.
    let fresh = copy(config, State::default());
    assert_eq!(
        Superblock::select(&fresh, &fresh),
        Ok((config, State::default()))
    );
}

#[test]
fn select_falls_back_when_either_copy_is_torn() {
    let config = superblock(128);
    let (older, newer) = (copy(config, state(4)), copy(config, state(5)));
    let mut flipped = newer.clone();
    flipped[30] ^= 0x04;
    let zeroed = vec![0; COPY_BYTES];
    let mut truncated = newer.clone();
    truncated[ENCODED_BYTES / 2..].fill(0);
    for torn in [&flipped, &zeroed, &truncated] {
        assert_eq!(Superblock::select(&older, torn), Ok((config, state(4))));
        assert_eq!(Superblock::select(torn, &older), Ok((config, state(4))));
    }
    assert_eq!(
        Superblock::select(&zeroed, &flipped),
        Err(StoreError::NoSuperblock)
    );
}

#[test]
fn copies_that_disagree_on_the_configuration_are_rejected() {
    let other = Superblock {
        max_key_len: 9,
        ..superblock(128)
    };
    assert_eq!(
        Superblock::select(&copy(superblock(128), state(1)), &copy(other, state(2))),
        Err(StoreError::InvalidSuperblock(
            "superblock copies disagree on the configuration"
        ))
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
