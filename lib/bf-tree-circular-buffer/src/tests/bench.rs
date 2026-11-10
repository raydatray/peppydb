//! Informational cost measurement; run in release mode:
//!
//! ```sh
//! tools/buck2 run @build/mode/release \
//!     //lib/bf-tree-circular-buffer:bf-tree-circular-buffer-unittest \
//!     -- --ignored --nocapture tests::bench::
//! ```

use std::time::Instant;

use super::*;

/// Average cost of one `alloc` plus one `free` with `live` blocks held,
/// single-threaded. Each round frees a random held block and allocates a
/// replacement of a random class, so holes and the head both stay active.
#[test]
#[ignore = "benchmark"]
fn circular_buffer_cost() {
    let classes = [64, 128, 256, 512];
    for live in [10_000, 100_000, 300_000] {
        // Room for every live block at the largest class, plus slack.
        let capacity = (live * (BLOCK_HEADER_BYTES + 512) * 2).next_power_of_two();
        let buffer = CircularBuffer::new(capacity, &classes, 10).unwrap();
        let mut rng = Rng::new(live as u64);
        let mut held: Vec<_> = (0..live)
            .map(|i| {
                let size = classes[rng.below(classes.len())];
                buffer.alloc(size, Kind::Mini, leaf(i as u64)).unwrap()
            })
            .collect();

        let rounds = 1_000_000;
        let start = Instant::now();
        for i in 0..rounds {
            let index = rng.below(held.len());
            let size = classes[rng.below(classes.len())];
            let block = buffer.alloc(size, Kind::Mini, leaf(i as u64)).unwrap();
            buffer.free(std::mem::replace(&mut held[index], block));
        }
        let elapsed = start.elapsed();

        let metrics = buffer.metrics();
        println!(
            "circular_buffer_cost: live {live:>7}: {:>6.1} ns per alloc + free \
             ({} reuses, {} tail allocations, {} tombstones)",
            elapsed.as_nanos() as f64 / rounds as f64,
            metrics.reuses,
            metrics.allocs - live as u64,
            metrics.tombstones,
        );
        held.into_iter().for_each(|block| buffer.free(block));
        assert_eq!(buffer.used(), 0);
    }
}

/// Size-class free lists against tombstone-on-free, under a synthetic
/// leaf trace with eviction (plan 0023). Owners are leaves with a skewed
/// access pattern (80% of steps on 20% of leaves). A step on a leaf without
/// a block allocates the smallest class; otherwise the block grows by one
/// class (free, then allocate), and a block at the largest class is freed,
/// as a merge into the base page frees it. A failed allocation evicts the
/// oldest block, as the tree does. Reports steps per second, eviction
/// count, and budget utilisation (live bytes over capacity, sampled).
#[test]
#[ignore = "benchmark"]
fn bench_circular_buffer_free() {
    let classes = [256, 512, 1024, 2048, 4096];
    let capacity = 16 << 20;
    let owners = 50_000;
    let steps = 3_000_000;
    for holes in [true, false] {
        let mut buffer = CircularBuffer::new(capacity, &classes, 10).unwrap();
        if !holes {
            buffer = buffer.without_holes();
        }
        let mut rng = Rng::new(42);
        let mut held: Vec<Option<(Block, usize)>> = (0..owners).map(|_| None).collect();
        let mut live_bytes = 0usize;
        let (mut utilisation, mut samples) = (0.0, 0u64);
        let start = Instant::now();
        for step in 0..steps {
            let owner = if rng.below(100) < 80 {
                rng.below(owners / 5)
            } else {
                rng.below(owners)
            };
            let class = match held[owner].take() {
                None => Some(0),
                Some((block, class)) => {
                    live_bytes -= BLOCK_HEADER_BYTES + classes[class];
                    buffer.free(block);
                    (class + 1 < classes.len()).then_some(class + 1)
                }
            };
            if let Some(class) = class {
                let block = loop {
                    match buffer.alloc(classes[class], Kind::Mini, leaf(owner as u64 + 1)) {
                        Ok(block) => break block,
                        Err(CircularBufferFull) => match buffer.claim_victim() {
                            Claim::Victim(victim) => {
                                let index = victim.owner.raw() as usize - 1;
                                let (block, class) = held[index].take().expect("a live owner");
                                live_bytes -= BLOCK_HEADER_BYTES + classes[class];
                                buffer.free(block);
                                buffer.retire(victim);
                            }
                            other => panic!("no victim in a full buffer: {other:?}"),
                        },
                    }
                };
                live_bytes += BLOCK_HEADER_BYTES + classes[class];
                held[owner] = Some((block, class));
            }
            if step % 1024 == 0 {
                utilisation += live_bytes as f64 / capacity as f64;
                samples += 1;
            }
        }
        let elapsed = start.elapsed();
        let metrics = buffer.metrics();
        println!(
            "bench_circular_buffer_free: {:<16} {:>6.1} ns per step, {:>7} evictions, \
             {:>5.1}% utilisation ({} reuses, {} tombstones)",
            if holes {
                "free lists"
            } else {
                "tombstone-on-free"
            },
            elapsed.as_nanos() as f64 / steps as f64,
            metrics.retired,
            100.0 * utilisation / samples as f64,
            metrics.reuses,
            metrics.tombstones,
        );
        held.into_iter()
            .flatten()
            .for_each(|(block, _)| buffer.free(block));
    }
}
