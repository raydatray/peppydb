use super::*;

const H: usize = BLOCK_HEADER_BYTES;

/// Allocate `n` blocks of `size` bytes for leaves `0..n`.
fn fill(buffer: &CircularBuffer, size: usize, n: u64) -> Vec<Block> {
    (0..n)
        .map(|i| buffer.alloc(size, Kind::Mini, leaf(i)).unwrap())
        .collect()
}

#[test]
fn a_request_reuses_a_hole_of_its_own_class() {
    let buffer = CircularBuffer::new(2048, &[64, 128], 0).unwrap();
    let mut blocks = fill(&buffer, 64, 3);
    let tail = buffer.used();

    // The middle block is neither at the head nor in a region (percent 0).
    let mut middle = blocks.remove(1);
    let addr = middle.addr();
    middle.as_mut().fill(0xAB);
    buffer.free(middle);
    assert_eq!(buffer.used(), tail, "a hole keeps its space");

    let reused = buffer.alloc(64, Kind::Mini, leaf(7)).unwrap();
    assert_eq!(reused.addr(), addr);
    assert_eq!(reused.as_ref().len(), 64);
    assert!(
        reused.as_ref().iter().all(|&byte| byte == 0),
        "reused holes are zeroed"
    );
    assert_eq!(buffer.used(), tail);
    assert_eq!((buffer.metrics().allocs, buffer.metrics().reuses), (3, 1));

    blocks.push(reused);
    blocks.into_iter().for_each(|block| buffer.free(block));
    assert_eq!(buffer.used(), 0);
}

#[test]
fn a_request_does_not_take_a_hole_of_a_larger_class() {
    let buffer = CircularBuffer::new(2048, &[64, 128], 0).unwrap();
    let first = buffer.alloc(64, Kind::Mini, leaf(0)).unwrap();
    let large = buffer.alloc(128, Kind::Mini, leaf(1)).unwrap();
    let last = buffer.alloc(64, Kind::Mini, leaf(2)).unwrap();
    let large_addr = large.addr();
    buffer.free(large);

    // Only a 128-byte hole is free: a 64-byte request bumps the tail.
    let tail = buffer.used();
    let small = buffer.alloc(64, Kind::Mini, leaf(3)).unwrap();
    assert_eq!(small.addr(), tail);
    assert_eq!(buffer.metrics().reuses, 0);

    // A 128-byte request takes the hole.
    let again = buffer.alloc(128, Kind::Mini, leaf(4)).unwrap();
    assert_eq!(again.addr(), large_addr);
    assert_eq!(buffer.metrics().reuses, 1);

    [first, last, small, again]
        .into_iter()
        .for_each(|block| buffer.free(block));
    assert_eq!(buffer.used(), 0);
}

#[test]
fn a_freed_block_in_the_region_becomes_a_tombstone() {
    // 50 percent of 1024: blocks at least 512 bytes behind the tail.
    let buffer = CircularBuffer::new(1024, &[112], 50).unwrap();
    let mut blocks = fill(&buffer, 112, 6);
    // Tail 768. Block 1 (addr 128) is 640 behind: in the region.
    // Block 4 (addr 512) is 256 behind: outside it.
    assert!(buffer.in_second_chance(blocks[1].addr()));
    assert!(!buffer.in_second_chance(blocks[4].addr()));

    let outside = blocks.remove(4);
    let inside = blocks.remove(1);
    buffer.free(inside);
    buffer.free(outside);
    assert_eq!(buffer.metrics().tombstones, 1);

    // Only the hole outside the region is reused.
    let reused = buffer.alloc(112, Kind::Mini, leaf(9)).unwrap();
    assert_eq!(reused.addr(), 4 * (H + 112));
    assert_eq!(buffer.metrics().reuses, 1);

    blocks.push(reused);
    blocks.into_iter().for_each(|block| buffer.free(block));
    assert_eq!(buffer.used(), 0);
}

#[test]
fn a_hole_that_ages_into_the_region_is_tombstoned_instead_of_reused() {
    let buffer = CircularBuffer::new(1024, &[112, 240], 50).unwrap();
    let mut blocks = fill(&buffer, 112, 3);
    // Tail 384; block 1 (addr 128) is 256 behind, outside the region.
    let hole = blocks.remove(1);
    let hole_addr = hole.addr();
    buffer.free(hole);
    assert_eq!(buffer.metrics().tombstones, 0);

    // A larger block, which cannot take the hole, moves the tail to 640: the
    // hole is now 512 behind, in the region.
    blocks.push(buffer.alloc(240, Kind::Mini, leaf(8)).unwrap());
    assert!(buffer.in_second_chance(hole_addr));

    let next = buffer.alloc(112, Kind::Mini, leaf(9)).unwrap();
    assert_eq!(next.addr(), 640);
    assert_eq!(buffer.metrics().reuses, 0);
    assert_eq!(buffer.metrics().tombstones, 1);

    blocks.push(next);
    blocks.into_iter().for_each(|block| buffer.free(block));
    assert_eq!(buffer.used(), 0);
}

#[test]
fn percent_zero_reuses_every_hole_and_percent_100_none() {
    for (percent, reuses) in [(0, 1), (100, 0)] {
        let buffer = CircularBuffer::new(1024, &[64], percent).unwrap();
        let mut blocks = fill(&buffer, 64, 3);
        assert_eq!(buffer.in_second_chance(blocks[2].addr()), percent == 100);
        buffer.free(blocks.remove(1));
        blocks.push(buffer.alloc(64, Kind::Mini, leaf(9)).unwrap());
        assert_eq!(buffer.metrics().reuses, reuses, "percent {percent}");
        blocks.into_iter().for_each(|block| buffer.free(block));
        assert_eq!(buffer.used(), 0);
    }
}

#[test]
fn a_freed_head_block_is_never_a_hole() {
    let buffer = CircularBuffer::new(1024, &[64], 0).unwrap();
    let blocks = fill(&buffer, 64, 2);
    let mut blocks = blocks.into_iter();
    buffer.free(blocks.next().unwrap());
    assert_eq!(buffer.metrics().tombstones, 1);
    assert_eq!(buffer.used(), H + 64);
    buffer.free(blocks.next().unwrap());
}

#[test]
fn stale_entries_are_never_handed_out_and_are_compacted() {
    let buffer = CircularBuffer::new(4096, &[64], 0).unwrap();
    let mut blocks = fill(&buffer, 64, 9);
    // Holes at 1, 3, 5, 7 (behind live blocks, outside any region).
    for i in [7, 5, 3, 1] {
        buffer.free(blocks.remove(i));
    }
    assert_eq!(buffer.lock_state().free_entries(), 4);

    // Freeing block 0 lets the head pass the hole behind it; freeing block 2
    // passes another. Two of four entries are stale: not yet more than half.
    let mut blocks = blocks.into_iter();
    buffer.free(blocks.next().unwrap());
    buffer.free(blocks.next().unwrap());
    let head = buffer.lock_state().head;
    assert_eq!(head, 4 * (H + 64));
    assert_eq!(buffer.lock_state().free_entries(), 4);

    // A third passed hole tips the balance, and the lists are filtered.
    buffer.free(blocks.next().unwrap());
    assert_eq!(buffer.lock_state().free_entries(), 1);

    // The remaining hole (at block 7) is reused; passed holes are not.
    let reused = buffer.alloc(64, Kind::Mini, leaf(9)).unwrap();
    assert_eq!(reused.addr(), 7 * (H + 64));
    let fresh = buffer.alloc(64, Kind::Mini, leaf(10)).unwrap();
    assert_eq!(fresh.addr(), 9 * (H + 64));

    blocks
        .chain([reused, fresh])
        .for_each(|block| buffer.free(block));
    assert_eq!(buffer.used(), 0);
}

#[test]
#[should_panic(expected = "is not a class")]
fn a_size_that_is_not_a_class_panics() {
    let buffer = CircularBuffer::new(1024, &[64, 128], 0).unwrap();
    let _ = buffer.alloc(96, Kind::Mini, leaf(1));
}
