//! A crash model against an oracle (plan 0018), over `SimStore`
//! (`bf-tree-store`, feature `sim`; plan 0030 moved plan 0018's
//! `FaultStore` there).
//!
//! The store keeps a durable image and the writes since the last sync.
//! `sync` and `commit` make writes durable. A crash keeps the durable image
//! and loses, lands, or tears each later write at 64-byte sectors. The store
//! can also fail a commit before or after its superblock copy lands, and it
//! can fail every call after the `k`-th, like a process that dies in the
//! middle of an operation (a pass, a split, an erase on open).

use std::collections::BTreeMap;

use super::*;
pub(super) use crate::CommitFault;
pub(super) use crate::SimStore as FaultStore;

/// A new store for `config`.
pub(super) fn new_store(config: &TreeConfig) -> FaultStore {
    FaultStore::new(Superblock::from(config)).unwrap()
}

/// The store after a crash, with each choice drawn from `rng`.
pub(super) fn crashed(store: &FaultStore, rng: &mut Rng) -> FaultStore {
    FaultStore::from_image(&store.crash_with(|bound| rng.below(bound)))
}

/// A deterministic generator.
pub(super) struct Rng(pub(super) u64);

impl Rng {
    pub(super) fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

#[derive(Clone, Debug)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Remove(Vec<u8>),
}

/// Every applied operation with its sequence, in sequence order.
#[derive(Default)]
struct Log(Vec<(u64, Op)>);

impl Log {
    /// The map that the operations at or below `seq` produce.
    fn state_at(&self, seq: u64) -> BTreeMap<Vec<u8>, Vec<u8>> {
        let mut map = BTreeMap::new();
        for (_, op) in self.0.iter().take_while(|(s, _)| *s <= seq) {
            match op {
                Op::Put(key, value) => {
                    map.insert(key.clone(), value.clone());
                }
                Op::Remove(key) => {
                    map.remove(key);
                }
            }
        }
        map
    }
}

/// Apply `op` to the tree; on success, log it with its sequence. A store
/// failure may reject it; it is then not applied.
fn apply(tree: &Tree, log: &mut Log, op: Op) -> bool {
    let result = match &op {
        Op::Put(key, value) => tree.put(key, value),
        Op::Remove(key) => tree.remove(key),
    };
    match result {
        Ok(()) => {
            log.0.push((tree.applied_seq(), op));
            true
        }
        Err(_) => false,
    }
}

fn random_op(rng: &mut Rng, config: &TreeConfig) -> Op {
    let key = (rng.below(400) as u32).to_be_bytes().to_vec();
    if rng.below(5) == 0 {
        Op::Remove(key)
    } else {
        let len = rng.below(config.max_value_len as u64 + 1) as usize;
        Op::Put(key, vec![rng.below(256) as u8; len])
    }
}

fn contents(tree: &Tree) -> BTreeMap<Vec<u8>, Vec<u8>> {
    tree.iter().map(Result::unwrap).collect()
}

/// Open the crashed disk, possibly dying during the open itself (for
/// example while it erases late pages); a death means another crash.
pub(super) fn recover(store: FaultStore, config: TreeConfig, rng: &mut Rng) -> (Tree, FaultStore) {
    let mut store = store;
    loop {
        if rng.below(4) == 0 {
            store.die_after(rng.below(40));
        }
        match Tree::open_with(config, Box::new(store.clone())) {
            Ok(tree) => {
                store.live_on();
                return (tree, store);
            }
            Err(TreeError::Store(_)) => store = crashed(&store, rng),
            Err(error) => panic!("open after a crash failed: {error:?}"),
        }
    }
}

/// One crash model run: `rounds` generations of writes, checkpoints, and a
/// crash, each recovered from the crashed disk itself.
fn run(config: TreeConfig, seed: u64, rounds: usize, ops: usize) {
    let mut rng = Rng(seed | 1);
    let store = new_store(&config);
    let mut tree = Tree::create(config, Box::new(store.clone())).unwrap();
    let mut store = store;
    let mut log = Log::default();
    for round in 0..rounds {
        let context = format!("seed {seed:#x} round {round}");
        // Writes and checkpoints, then a crash in one of three ways.
        let crash_at = rng.below(ops as u64) as usize;
        let mut last_commit = store.durable_state();
        for step in 0..ops {
            if step == crash_at {
                match rng.below(3) {
                    0 => {}
                    1 => store.die_after(rng.below(60)),
                    _ => store.fail_next_commit(if rng.below(2) == 0 {
                        CommitFault::BeforeCopy
                    } else {
                        CommitFault::AfterCopy
                    }),
                }
            }
            if rng.below(25) == 0 {
                if let Ok(state) = tree.checkpoint() {
                    last_commit = state;
                }
            } else {
                apply(&tree, &mut log, random_op(&mut rng, &config));
            }
            if step > crash_at && (tree.store_failed() || rng.below(8) == 0) {
                break;
            }
        }

        // Crash and recover.
        drop(tree);
        let crashed = crashed(&store, &mut rng);
        let (recovered, recovered_store) = recover(crashed, config, &mut rng);
        tree = recovered;
        store = recovered_store;
        let applied = tree.applied_seq();
        let durable = store.durable_state();
        assert_eq!(applied, durable.applied_seq, "{context}");
        assert!(
            applied >= last_commit.applied_seq,
            "{context}: an acknowledged commit was lost"
        );
        assert_eq!(contents(&tree), log.state_at(applied), "{context}");
        assert_eq!(tree.check_invariants(), Ok(()), "{context}");

        // Replay the lost operations: all, none, or a prefix. The log is
        // truncated to match, and replayed operations get new sequences.
        let lost: Vec<Op> = log
            .0
            .iter()
            .filter(|(seq, _)| *seq > applied)
            .map(|(_, op)| op.clone())
            .collect();
        log.0.retain(|(seq, _)| *seq <= applied);
        let replay = match rng.below(4) {
            0 => 0,
            1 => rng.below(lost.len() as u64 + 1) as usize,
            _ => lost.len(),
        };
        for op in lost.into_iter().take(replay) {
            assert!(apply(&tree, &mut log, op), "{context}: replay");
        }
        assert_eq!(contents(&tree), log.state_at(u64::MAX), "{context}");
    }
}

/// Tiny nodes and a circular buffer of about four pages, so most
/// write-throughs come from eviction, across many seeds.
#[test]
fn crashes_recover_the_committed_state_with_a_tiny_buffer() {
    let config = TreeConfig {
        circular_buffer_bytes: 1024,
        ..config(256, 128, 8, 16)
    };
    for seed in 0..24 {
        run(config, 0xc4a5 + seed * 7919, 12, 300);
    }
}

/// A larger circular buffer: write-throughs come from checkpoints and
/// merges, and full pages and caching are on.
#[test]
fn crashes_recover_the_committed_state_with_a_large_buffer() {
    let config = TreeConfig {
        circular_buffer_bytes: 64 << 10,
        write_load_full_page: true,
        read_promotion_percent: 20,
        ..config(256, 128, 8, 16)
    };
    for seed in 0..12 {
        run(config, 0x0b16 + seed * 104_729, 10, 400);
    }
}

/// Writers on disjoint keys and a checkpoint thread, with the store dying
/// at a random call. After recovery each key holds the oracle's value at
/// some sequence in `[N, last assigned]` (a fuzzy checkpoint), and replaying
/// every operation above `N` in sequence order gives the full oracle.
#[test]
fn crashes_with_checkpoints_beside_writers_recover_after_replay() {
    let config = TreeConfig {
        circular_buffer_bytes: 2048,
        ..config(256, 128, 8, 16)
    };
    for seed in 0..12u64 {
        super::concurrent::with_watchdog(std::time::Duration::from_secs(60), move || {
            crash_beside_writers(config, seed);
        });
    }
}

/// `put` or `remove` as the tree does it, returning the sequence.
fn apply_seq(tree: &Tree, op: &Op) -> Result<u64, TreeError> {
    let state = match op {
        Op::Put(_, value) => bf_tree_page::RecordState::Insert(value),
        Op::Remove(_) => bf_tree_page::RecordState::Delete,
    };
    let key = match op {
        Op::Put(key, _) | Op::Remove(key) => key,
    };
    let seq = tree.write(key, state, &mut || {
        Ok(tree
            .last_seq
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1)
    })?;
    tree.applied
        .fetch_max(seq, std::sync::atomic::Ordering::Release);
    Ok(seq)
}

fn crash_beside_writers(config: TreeConfig, seed: u64) {
    let mut rng = Rng(0xfeed + seed * 6151);
    let store = new_store(&config);
    let tree = Tree::create(config, Box::new(store.clone())).unwrap();
    store.die_after(200 + rng.below(3000));
    let mut log: Vec<(u64, Op)> = std::thread::scope(|scope| {
        let tree = &tree;
        let writers: Vec<_> = (0..4u8)
            .map(|w| {
                scope.spawn(move || {
                    let mut rng = Rng(seed * 31 + u64::from(w) + 1);
                    let mut log = Vec::new();
                    for _ in 0..600 {
                        let key = vec![rng.below(60) as u8, w];
                        let op = if rng.below(5) == 0 {
                            Op::Remove(key)
                        } else {
                            let len = rng.below(17) as usize;
                            Op::Put(key, vec![rng.below(256) as u8; len])
                        };
                        if let Ok(seq) = apply_seq(tree, &op) {
                            log.push((seq, op));
                        }
                    }
                    log
                })
            })
            .collect();
        let checkpointer = scope.spawn(move || while tree.checkpoint().is_ok() {});
        let log = writers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect();
        // Writers are done; stop the checkpoints if the store still lives.
        store.die_after(0);
        checkpointer.join().unwrap();
        log
    });
    log.sort_by_key(|(seq, _)| *seq);
    let log = Log(log);
    let last = log.0.last().map_or(0, |(seq, _)| *seq);
    drop(tree);

    let crashed = crashed(&store, &mut rng);
    let (tree, _) = recover(crashed, config, &mut rng);
    let applied = tree.applied_seq();
    let context = format!("seed {seed} applied {applied} last {last}");

    // Per key: the value is the oracle's value at some sequence in
    // [N, last]: after every operation on the key at or below N, or after
    // one later operation on it.
    let base = log.state_at(applied);
    let found = contents(&tree);
    let keys: std::collections::BTreeSet<_> = log
        .0
        .iter()
        .map(|(_, op)| match op {
            Op::Put(key, _) | Op::Remove(key) => key.clone(),
        })
        .collect();
    for key in &keys {
        let mut allowed = vec![base.get(key).cloned()];
        allowed.extend(log.0.iter().filter(|(seq, _)| *seq > applied).filter_map(
            |(_, op)| match op {
                Op::Put(k, value) if k == key => Some(Some(value.clone())),
                Op::Remove(k) if k == key => Some(None),
                _ => None,
            },
        ));
        let value = found.get(key).cloned();
        assert!(
            allowed.contains(&value),
            "{context}: key {key:?} holds {value:?}"
        );
    }
    assert!(found.keys().all(|key| keys.contains(key)), "{context}");
    assert_eq!(tree.check_invariants(), Ok(()), "{context}");

    // Replay every operation above N in sequence order.
    for (_, op) in log.0.iter().filter(|(seq, _)| *seq > applied) {
        apply_seq(&tree, op).unwrap();
    }
    assert_eq!(contents(&tree), log.state_at(u64::MAX), "{context}");
}
