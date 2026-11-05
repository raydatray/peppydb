//! Threads allocating, writing, freeing, claiming, and retiring against one
//! small buffer.

use super::*;

/// One thread's loop. Thread `t` owns leaves `t * 1_000_000 + n` and writes a
/// pattern derived from each block's owner, so any overlap with another
/// thread's block shows up as changed bytes.
fn worker(buffer: &CircularBuffer, thread: u64, ops: usize, classes: &[usize]) {
    let mut rng = Rng::new(thread + 1);
    let mut held: Vec<(Block, u8)> = Vec::new();
    let base = thread * 1_000_000;

    for n in 0..ops as u64 {
        assert!(buffer.used() <= buffer.capacity());
        match rng.below(10) {
            0..5 => {
                let size = classes[rng.below(classes.len())];
                match buffer.alloc(size, Kind::Mini, leaf(base + n)) {
                    Ok(mut block) => {
                        let pattern = (thread as u8).wrapping_mul(31).wrapping_add(n as u8) | 1;
                        assert!(block.as_ref().iter().all(|&b| b == 0));
                        block.as_mut().fill(pattern);
                        held.push((block, pattern));
                    }
                    Err(CircularBufferFull) => make_room(buffer, base, &mut held),
                }
            }
            5..8 if !held.is_empty() => {
                let (block, pattern) = held.swap_remove(rng.below(held.len()));
                assert!(
                    block.as_ref().iter().all(|&b| b == pattern),
                    "bytes changed"
                );
                buffer.free(block);
            }
            _ => evict_one(buffer, base, &mut held),
        }
    }

    for (block, pattern) in held {
        assert!(
            block.as_ref().iter().all(|&b| b == pattern),
            "bytes changed"
        );
        buffer.free(block);
    }
}

/// Claim the oldest block. Evict it if this thread owns it; otherwise give
/// the claim back (its owner may free it meanwhile, which unclaim handles).
fn evict_one(buffer: &CircularBuffer, base: u64, held: &mut Vec<(Block, u8)>) {
    let Claim::Victim(victim) = buffer.claim_victim() else {
        return;
    };
    let mine = (base..base + 1_000_000).contains(&victim.owner.raw());
    match held.iter().position(|(block, _)| block.is(&victim)) {
        Some(index) if mine => {
            let (block, pattern) = held.swap_remove(index);
            assert!(
                block.as_ref().iter().all(|&b| b == pattern),
                "bytes changed"
            );
            buffer.free(block);
            buffer.retire(victim);
        }
        _ => buffer.unclaim(victim),
    }
}

/// After `CircularBufferFull`: try an eviction, then free one of this
/// thread's blocks, so that every thread keeps making progress.
fn make_room(buffer: &CircularBuffer, base: u64, held: &mut Vec<(Block, u8)>) {
    evict_one(buffer, base, held);
    if let Some((block, _)) = held.pop() {
        buffer.free(block);
    } else {
        std::thread::yield_now();
    }
}

#[test]
fn threads_share_one_small_buffer() {
    let (threads, ops) = if cfg!(miri) { (2, 60) } else { (4, 20_000) };
    let classes = [64, 128, 256];

    for percent in [0, 50] {
        let buffer = CircularBuffer::new(2048, &classes, percent).unwrap();
        std::thread::scope(|scope| {
            for thread in 0..threads {
                let buffer = &buffer;
                scope.spawn(move || worker(buffer, thread, ops, &classes));
            }
        });

        assert_eq!(buffer.used(), 0);
        assert_eq!(buffer.claim_victim(), Claim::Empty);
        let metrics = buffer.metrics();
        assert_eq!(metrics.allocs + metrics.reuses, metrics.frees);
    }
}
