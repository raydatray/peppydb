//! The circular buffer against an independent model.
//!
//! The model is a map from logical address to block, written from the
//! specification rather than from the implementation: the physical budget,
//! wrap fillers, LIFO reuse of same-class holes outside the second-chance
//! region, oldest-first claims, and the head passing dead blocks. Random
//! operations run against both, and every outcome must match exactly.

use std::collections::BTreeMap;

use super::*;

const H: usize = BLOCK_HEADER_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MState {
    Live,
    Claimed { freed: bool },
    Freed,
    Tombstone,
    Evicted,
    Filler,
}

#[derive(Clone, Copy, Debug)]
struct MBlock {
    span: usize,
    state: MState,
    kind: Kind,
    owner: Option<LeafId>,
}

struct Model {
    capacity: usize,
    classes: Vec<usize>,
    /// Bytes behind the tail at which the region starts; `usize::MAX` if none.
    threshold: usize,
    head: usize,
    tail: usize,
    blocks: BTreeMap<usize, MBlock>,
    /// Per class, the addresses of freed holes, most recent last.
    holes: Vec<Vec<usize>>,
    metrics: Metrics,
}

#[derive(Debug, PartialEq, Eq)]
enum MClaim {
    Victim {
        addr: usize,
        owner: LeafId,
        kind: Kind,
    },
    Busy,
    Empty,
}

impl Model {
    fn new(capacity: usize, classes: &[usize], percent: u32) -> Self {
        let threshold = if percent == 0 {
            usize::MAX
        } else {
            capacity * (100 - percent as usize) / 100
        };
        Self {
            capacity,
            classes: classes.to_vec(),
            threshold,
            head: 0,
            tail: 0,
            blocks: BTreeMap::new(),
            holes: vec![Vec::new(); classes.len()],
            metrics: Metrics::default(),
        }
    }

    fn in_region(&self, addr: usize) -> bool {
        self.tail - addr >= self.threshold
    }

    fn is_dead(state: MState) -> bool {
        matches!(
            state,
            MState::Freed | MState::Tombstone | MState::Evicted | MState::Filler
        )
    }

    fn advance(&mut self) {
        while let Some((&addr, block)) = self.blocks.first_key_value()
            && Self::is_dead(block.state)
        {
            assert_eq!(addr, self.head);
            self.head += block.span;
            self.blocks.remove(&addr);
        }
        if self.blocks.is_empty() {
            assert_eq!(self.head, self.tail);
        }
    }

    fn alloc(&mut self, size: usize, kind: Kind, owner: LeafId) -> Option<usize> {
        let class = self.classes.iter().position(|&c| c == size).unwrap();
        while let Some(addr) = self.holes[class].pop() {
            if addr < self.head {
                continue; // passed by the head
            }
            if self.in_region(addr) {
                self.blocks.get_mut(&addr).unwrap().state = MState::Tombstone;
                self.metrics.tombstones += 1;
                continue;
            }
            let block = self.blocks.get_mut(&addr).unwrap();
            assert_eq!(block.state, MState::Freed);
            *block = MBlock {
                span: H + size,
                state: MState::Live,
                kind,
                owner: Some(owner),
            };
            self.metrics.reuses += 1;
            return Some(addr);
        }

        let span = H + size;
        let offset = self.tail % self.capacity;
        let filler = if offset + span > self.capacity {
            self.capacity - offset
        } else {
            0
        };
        if self.tail - self.head + filler + span > self.capacity {
            return None;
        }
        if filler > 0 {
            let block = MBlock {
                span: filler,
                state: MState::Filler,
                kind,
                owner: None,
            };
            self.blocks.insert(self.tail, block);
            self.tail += filler;
            self.metrics.fillers += 1;
        }
        let addr = self.tail;
        let block = MBlock {
            span,
            state: MState::Live,
            kind,
            owner: Some(owner),
        };
        self.blocks.insert(addr, block);
        self.tail += span;
        self.metrics.allocs += 1;
        Some(addr)
    }

    fn free(&mut self, addr: usize) {
        let at_head = self.blocks.first_key_value().map(|(&a, _)| a) == Some(addr);
        let in_region = self.in_region(addr);
        let block = self.blocks.get_mut(&addr).unwrap();
        match block.state {
            MState::Claimed { freed: false } => block.state = MState::Claimed { freed: true },
            MState::Live if at_head || in_region => {
                block.state = MState::Tombstone;
                self.metrics.tombstones += 1;
            }
            MState::Live => {
                block.state = MState::Freed;
                let class = self
                    .classes
                    .iter()
                    .position(|&c| H + c == block.span)
                    .unwrap();
                self.holes[class].push(addr);
            }
            other => panic!("model: free of a {other:?} block"),
        }
        self.metrics.frees += 1;
        self.advance();
    }

    fn claim(&mut self) -> MClaim {
        self.advance();
        let mut busy = false;
        for (&addr, block) in &mut self.blocks {
            match block.state {
                MState::Live => {
                    block.state = MState::Claimed { freed: false };
                    return MClaim::Victim {
                        addr,
                        owner: block.owner.unwrap(),
                        kind: block.kind,
                    };
                }
                MState::Claimed { .. } => busy = true,
                _ => {}
            }
        }
        if busy { MClaim::Busy } else { MClaim::Empty }
    }

    fn unclaim(&mut self, addr: usize) {
        let block = self.blocks.get_mut(&addr).unwrap();
        match block.state {
            MState::Claimed { freed: false } => block.state = MState::Live,
            MState::Claimed { freed: true } => self.retire(addr),
            other => panic!("model: unclaim of a {other:?} block"),
        }
    }

    fn retire(&mut self, addr: usize) {
        let block = self.blocks.get_mut(&addr).unwrap();
        assert_eq!(block.state, MState::Claimed { freed: true });
        block.state = MState::Evicted;
        self.metrics.retired += 1;
        self.advance();
    }
}

/// A block the test holds, with the byte it filled the block with.
struct Held {
    block: Block,
    fill: u8,
    size: usize,
}

struct Run {
    buffer: CircularBuffer,
    model: Model,
    held: Vec<Held>,
    /// Claims in progress, and whether their block was freed.
    claims: Vec<(Victim, bool)>,
    rng: Rng,
    next_fill: u8,
}

impl Run {
    fn step(&mut self) {
        match self.rng.below(100) {
            0..45 => self.alloc(),
            45..70 => self.free(),
            70..82 => self.claim(),
            82..88 => self.unclaim(),
            _ => self.retire(),
        }
        self.check();
    }

    fn alloc(&mut self) {
        let size = self.model.classes[self.rng.below(self.model.classes.len())];
        let kind = if self.rng.below(2) == 0 {
            Kind::Mini
        } else {
            Kind::Full
        };
        let owner = leaf(self.rng.below(1000) as u64);
        let expected = self.model.alloc(size, kind, owner);
        match self.buffer.alloc(size, kind, owner) {
            Ok(mut block) => {
                assert_eq!(Some(block.addr()), expected, "allocation address");
                assert_eq!(block.as_ref().len(), size);
                assert!(block.as_ref().iter().all(|&b| b == 0), "zeroed");
                self.next_fill = self.next_fill.wrapping_add(1).max(1);
                block.as_mut().fill(self.next_fill);
                self.held.push(Held {
                    block,
                    fill: self.next_fill,
                    size,
                });
            }
            Err(CircularBufferFull) => assert_eq!(expected, None, "unexpected Full"),
        }
    }

    fn free(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let held = self.held.swap_remove(self.rng.below(self.held.len()));
        if let Some(claim) = self.claims.iter_mut().find(|(v, _)| held.block.is(v)) {
            claim.1 = true;
        }
        self.model.free(held.block.addr());
        self.buffer.free(held.block);
    }

    fn claim(&mut self) {
        let expected = self.model.claim();
        let actual = self.buffer.claim_victim();
        match actual {
            Claim::Victim(v) => {
                let found = MClaim::Victim {
                    addr: v.addr,
                    owner: v.owner,
                    kind: v.kind,
                };
                assert_eq!(found, expected);
                assert!(self.held.iter().any(|h| h.block.is(&v)));
                self.claims.push((v, false));
            }
            Claim::Busy => assert_eq!(expected, MClaim::Busy),
            Claim::Empty => assert_eq!(expected, MClaim::Empty),
        }
    }

    fn unclaim(&mut self) {
        if self.claims.is_empty() {
            return;
        }
        let (victim, _) = self.claims.swap_remove(self.rng.below(self.claims.len()));
        self.model.unclaim(victim.addr);
        self.buffer.unclaim(victim);
    }

    /// The evictor's path: take the block from its holder if it is still
    /// held, free it, and retire the claim.
    fn retire(&mut self) {
        if self.claims.is_empty() {
            return;
        }
        let (victim, freed) = self.claims.swap_remove(self.rng.below(self.claims.len()));
        if !freed {
            let index = self.held.iter().position(|h| h.block.is(&victim)).unwrap();
            let held = self.held.swap_remove(index);
            self.model.free(held.block.addr());
            self.buffer.free(held.block);
        }
        self.model.retire(victim.addr);
        self.buffer.retire(victim);
    }

    fn check(&self) {
        let used = self.buffer.used();
        assert_eq!(used, self.model.tail - self.model.head, "used");
        assert!(used <= self.buffer.capacity());
        assert_eq!(self.buffer.metrics(), self.model.metrics, "metrics");

        // Held blocks sit at their logical address, inside the slab, do not
        // overlap, and keep their bytes.
        let capacity = self.buffer.capacity();
        let mut ranges: Vec<_> = self
            .held
            .iter()
            .map(|h| {
                let start = offset(&self.buffer, &h.block);
                assert_eq!(start, h.block.addr() % capacity);
                assert!(start + H + h.size <= capacity, "block crosses the end");
                assert!(
                    h.block.as_ref().iter().all(|&b| b == h.fill),
                    "bytes changed"
                );
                (start, start + H + h.size)
            })
            .collect();
        ranges.sort_unstable();
        assert!(
            ranges.windows(2).all(|w| w[0].1 <= w[1].0),
            "held blocks overlap"
        );
    }

    /// Return every block and finish every claim; the buffer must be empty.
    fn drain(mut self) {
        while !self.claims.is_empty() {
            self.retire();
        }
        while !self.held.is_empty() {
            self.free();
        }
        self.check();
        assert_eq!(self.buffer.used(), 0);
        assert_eq!(self.buffer.claim_victim(), Claim::Empty);
        let metrics = self.buffer.metrics();
        assert_eq!(metrics.allocs + metrics.reuses, metrics.frees);
    }
}

#[test]
fn the_buffer_agrees_with_the_model() {
    // From two to about eight largest blocks per buffer.
    let configs: &[(usize, &[usize])] = &[
        (256, &[48, 112]),
        (1024, &[64, 128, 256]),
        (2048, &[64, 128, 256]),
        (4096, &[64, 192, 448]),
    ];
    // Miri interprets slowly: one seed, two buffers, and two region sizes
    // still cover holes, tombstones, fillers, and claims.
    let (steps, seeds, configs, percents): (_, u64, _, &[u32]) = if cfg!(miri) {
        (150, 1, &configs[..2], &[0, 50])
    } else {
        (4000, 4, configs, &[0, 10, 50, 100])
    };

    for &(capacity, classes) in configs {
        for &percent in percents {
            for seed in 0..seeds {
                let mut run = Run {
                    buffer: CircularBuffer::new(capacity, classes, percent).unwrap(),
                    model: Model::new(capacity, classes, percent),
                    held: Vec::new(),
                    claims: Vec::new(),
                    rng: Rng::new(seed * 1000 + capacity as u64 + u64::from(percent)),
                    next_fill: 0,
                };
                for _ in 0..steps {
                    run.step();
                }
                run.drain();
            }
        }
    }
}
