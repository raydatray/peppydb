use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;

const H: usize = BLOCK_HEADER_BYTES;

#[test]
fn configuration_errors() {
    let invalid = |capacity, classes: &[usize], percent| {
        CircularBuffer::new(capacity, classes, percent).unwrap_err()
    };

    assert!(matches!(
        invalid(1024, &[], 0),
        CircularBufferError::InvalidClasses(_)
    ));
    assert!(matches!(
        invalid(1024, &[0, 64], 0),
        CircularBufferError::InvalidClasses(_)
    ));
    assert!(matches!(
        invalid(1024, &[64, 64], 0),
        CircularBufferError::InvalidClasses(_)
    ));
    assert!(matches!(
        invalid(1024, &[128, 64], 0),
        CircularBufferError::InvalidClasses(_)
    ));
    assert!(matches!(
        invalid(1000, &[64], 0),
        CircularBufferError::InvalidCapacity(_)
    ));
    // Twice (largest + header) is 2 * 272 = 544 > 512.
    assert!(matches!(
        invalid(512, &[256], 0),
        CircularBufferError::InvalidCapacity(_)
    ));
    assert_eq!(
        invalid(1024, &[64], 101),
        CircularBufferError::InvalidPercent(101)
    );

    // The boundaries themselves are accepted.
    CircularBuffer::new(512, &[240], 100).unwrap();
    CircularBuffer::new(512, &[64], 0).unwrap();
}

#[test]
fn blocks_are_exact_charged_and_inside_the_slab() {
    let buffer = CircularBuffer::new(1024, &[64, 128], 0).unwrap();
    let a = buffer.alloc(64, Kind::Mini, leaf(1)).unwrap();
    let b = buffer.alloc(128, Kind::Mini, leaf(2)).unwrap();

    assert_eq!(a.as_ref().len(), 64);
    assert_eq!(b.as_ref().len(), 128);
    assert_eq!(buffer.used(), (H + 64) + (H + 128));
    assert_eq!((a.addr(), b.addr()), (0, H + 64));
    assert_eq!(offset(&buffer, &b), H + 64);
    assert!(buffer.slab.contains(b.ptr(), b.span()));

    buffer.free(a);
    buffer.free(b);
    assert_eq!(buffer.used(), 0);
    assert_eq!(buffer.metrics().allocs, 2);
    assert_eq!(buffer.metrics().frees, 2);
}

#[test]
fn blocks_are_zeroed_on_every_lap() {
    let buffer = CircularBuffer::new(512, &[112], 0).unwrap();
    // Four 128-byte spans fill one lap, so 20 allocations cover five laps.
    for round in 0..20u8 {
        let mut block = buffer.alloc(112, Kind::Mini, leaf(1)).unwrap();
        assert!(
            block.as_ref().iter().all(|&byte| byte == 0),
            "round {round}"
        );
        block.as_mut().fill(round | 0x80);
        buffer.free(block);
    }
    assert_eq!(buffer.metrics().fillers, 0);
}

#[test]
fn writes_stay_inside_their_block() {
    let buffer = CircularBuffer::new(1024, &[64], 0).unwrap();
    let mut blocks: Vec<_> = (0..4)
        .map(|i| buffer.alloc(64, Kind::Mini, leaf(i)).unwrap())
        .collect();
    for (i, block) in blocks.iter_mut().enumerate() {
        block.as_mut().fill(i as u8 + 1);
    }
    for (i, block) in blocks.iter().enumerate() {
        assert!(block.as_ref().iter().all(|&byte| byte == i as u8 + 1));
    }
    blocks.into_iter().for_each(|block| buffer.free(block));
}

#[test]
fn a_block_that_would_cross_the_end_follows_a_filler() {
    let buffer = CircularBuffer::new(512, &[64, 192], 0).unwrap();
    let a = buffer.alloc(192, Kind::Mini, leaf(1)).unwrap();
    let b = buffer.alloc(192, Kind::Mini, leaf(2)).unwrap();
    buffer.free(a);
    buffer.free(b);
    assert_eq!(buffer.used(), 0);

    // The tail is at offset 416; 416 + 208 crosses 512, so a 96-byte filler
    // covers the rest of the slab and the block starts at offset 0.
    assert!(buffer.has_room(192));
    let c = buffer.alloc(192, Kind::Mini, leaf(3)).unwrap();
    assert_eq!(c.addr(), 512);
    assert_eq!(offset(&buffer, &c), 0);
    assert_eq!(buffer.used(), 96 + H + 192);
    assert_eq!(buffer.metrics().fillers, 1);

    // The filler is dead: freeing the block behind it empties the buffer.
    buffer.free(c);
    assert_eq!(buffer.used(), 0);
}

#[test]
fn full_exactly_at_the_budget() {
    let buffer = CircularBuffer::new(512, &[112], 0).unwrap();
    let blocks: Vec<_> = (0..4)
        .map(|i| buffer.alloc(112, Kind::Mini, leaf(i)).unwrap())
        .collect();
    assert_eq!(buffer.used(), buffer.capacity());
    assert!(!buffer.has_room(112));
    assert_eq!(
        buffer.alloc(112, Kind::Mini, leaf(9)).unwrap_err(),
        CircularBufferFull
    );
    assert_eq!(buffer.metrics().allocs, 4);

    blocks.into_iter().for_each(|block| buffer.free(block));
    assert!(buffer.has_room(112));
}

#[test]
fn has_room_counts_the_filler() {
    let buffer = CircularBuffer::new(512, &[64, 192], 0).unwrap();
    let small: Vec<_> = (0..3)
        .map(|i| buffer.alloc(64, Kind::Mini, leaf(i)).unwrap())
        .collect();
    let large = buffer.alloc(192, Kind::Mini, leaf(3)).unwrap();
    let mut small = small.into_iter();
    buffer.free(small.next().unwrap());
    buffer.free(small.next().unwrap());

    // Head 160, tail 448: 224 bytes are free, enough for a 208-byte block,
    // but at offset 448 it would cross 512 and needs a 64-byte filler first.
    assert_eq!(buffer.used(), 288);
    assert!(!buffer.has_room(192));
    assert_eq!(
        buffer.alloc(192, Kind::Mini, leaf(9)).unwrap_err(),
        CircularBufferFull
    );
    assert_eq!(
        buffer.metrics().fillers,
        0,
        "a failed allocation adds no filler"
    );

    // A 64-byte block also needs the filler, and both fit.
    assert!(buffer.has_room(64));
    let c = buffer.alloc(64, Kind::Mini, leaf(4)).unwrap();
    assert_eq!(offset(&buffer, &c), 0);
    assert_eq!(buffer.metrics().fillers, 1);

    buffer.free(small.next().unwrap());
    buffer.free(large);
    buffer.free(c);
    assert_eq!(buffer.used(), 0);
}

#[test]
fn the_head_advances_over_every_following_dead_block() {
    let buffer = CircularBuffer::new(1024, &[64], 0).unwrap();
    let a = buffer.alloc(64, Kind::Mini, leaf(1)).unwrap();
    let b = buffer.alloc(64, Kind::Mini, leaf(2)).unwrap();
    let c = buffer.alloc(64, Kind::Mini, leaf(3)).unwrap();

    // `b` is behind the head: it becomes a hole, which makes no room yet.
    buffer.free(b);
    assert_eq!(buffer.used(), 3 * (H + 64));

    // Freeing the head passes `a` (a tombstone) and the dead `b`.
    buffer.free(a);
    assert_eq!(buffer.used(), H + 64);
    assert_eq!(buffer.metrics().tombstones, 1);

    buffer.free(c);
    assert_eq!(buffer.used(), 0);
}

#[test]
fn freeing_a_block_of_another_buffer_panics() {
    let a = CircularBuffer::new(512, &[64], 0).unwrap();
    let b = CircularBuffer::new(512, &[64], 0).unwrap();
    let block = a.alloc(64, Kind::Mini, leaf(1)).unwrap();

    let result = catch_unwind(AssertUnwindSafe(|| b.free(block)));
    assert!(result.is_err());
    // `b` is unchanged and still usable.
    assert_eq!(b.used(), 0);
    let other = b.alloc(64, Kind::Mini, leaf(2)).unwrap();
    b.free(other);
    // The block went into the panicking call, so `a` keeps it outstanding.
    keep_alive(a);
}

#[test]
fn dropping_the_buffer_with_a_block_out_leaks_the_slab() {
    let buffer = CircularBuffer::new(512, &[64], 0).unwrap();
    let mut block = buffer.alloc(64, Kind::Mini, leaf(1)).unwrap();
    block.as_mut().fill(7);
    drop(buffer);

    // The bytes stay valid after the buffer is gone.
    assert!(block.as_ref().iter().all(|&byte| byte == 7));
    block.as_mut()[0] = 8;
    assert_eq!(block.as_ref()[0], 8);
    keep_alive(block);
}

#[test]
fn blocks_are_32_bytes_with_a_niche() {
    assert_eq!(size_of::<Block>(), 32);
    assert_eq!(size_of::<Option<Block>>(), 32);
}

#[test]
fn blocks_move_between_threads() {
    let buffer = CircularBuffer::new(1024, &[64], 0).unwrap();
    let mut block = buffer.alloc(64, Kind::Mini, leaf(1)).unwrap();
    let block = std::thread::scope(|scope| {
        scope
            .spawn(move || {
                block.as_mut().fill(3);
                block
            })
            .join()
            .unwrap()
    });
    assert!(block.as_ref().iter().all(|&byte| byte == 3));
    buffer.free(block);
}
