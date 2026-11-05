//! Informational cost measurement; run in release mode:
//!
//! ```sh
//! tools/buck2 run @build/mode/release \
//!     //crates/bf-tree-circular-buffer:bf-tree-circular-buffer-unittest \
//!     -- --ignored --nocapture circular_buffer_cost
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
