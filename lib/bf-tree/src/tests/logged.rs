//! Recovery with a log that loses unsynced records, and replica catch-up
//! (plan 0019).
//!
//! `LossyLog` keeps a durable prefix and a volatile tail of records. The
//! checkpoint hook syncs it through the captured sequence. A crash keeps
//! the durable prefix and a random prefix of the tail. After every crash,
//! opening the store and replaying the durable records above `N` must give
//! the state that the durable log alone describes: every batch is all or
//! nothing, and no committed effect lacks a durable record.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::crash::{CommitFault, FaultStore, Rng, recover};
use super::*;

/// One owned log record: a sequence and its operations.
type Record = (u64, Vec<(Vec<u8>, Option<Vec<u8>>)>);

#[derive(Default)]
struct Records {
    next: u64,
    durable: usize,
    all: Vec<Record>,
    fail_sync: bool,
}

#[derive(Default)]
struct LossyLog(Mutex<Records>);

fn owned(op: &Op<'_>) -> (Vec<u8>, Option<Vec<u8>>) {
    match *op {
        Op::Put { key, value } => (key.to_vec(), Some(value.to_vec())),
        Op::Delete { key } => (key.to_vec(), None),
    }
}

impl LossyLog {
    fn resume(records: Vec<Record>) -> Self {
        let next = records.last().map_or(0, |(seq, _)| *seq);
        Self(Mutex::new(Records {
            next,
            durable: records.len(),
            all: records,
            fail_sync: false,
        }))
    }

    /// Log a batch; the caller applies it.
    fn append_batch(&self, ops: &[Op<'_>]) -> u64 {
        let mut records = self.0.lock().unwrap();
        records.next += 1;
        let seq = records.next;
        records.all.push((seq, ops.iter().map(owned).collect()));
        seq
    }

    fn sync_to(&self, seq: u64) -> Result<(), LogError> {
        let mut records = self.0.lock().unwrap();
        if records.fail_sync {
            return Err(LogError::Other("log sync failed"));
        }
        let through = records.all.partition_point(|(s, _)| *s <= seq);
        records.durable = records.durable.max(through);
        Ok(())
    }

    /// The records that survive a crash: the durable prefix and a random
    /// prefix of the rest.
    fn crash(&self, rng: &mut Rng) -> Vec<Record> {
        let records = self.0.lock().unwrap();
        let extra = rng.below((records.all.len() - records.durable) as u64 + 1) as usize;
        records.all[..records.durable + extra].to_vec()
    }
}

impl Log for LossyLog {
    fn append(&self, op: Op<'_>) -> Result<u64, LogError> {
        let mut records = self.0.lock().unwrap();
        records.next += 1;
        let seq = records.next;
        records.all.push((seq, vec![owned(&op)]));
        Ok(seq)
    }
}

/// Apply one owned record as an entry.
fn apply_record(tree: &Tree, (seq, ops): &Record) -> Result<(), TreeError> {
    let ops: Vec<Op<'_>> = ops
        .iter()
        .map(|(key, value)| match value {
            Some(value) => Op::Put { key, value },
            None => Op::Delete { key },
        })
        .collect();
    tree.apply(Entry {
        seq: *seq,
        ops: &ops,
    })
}

/// The state the records describe.
fn state_of(records: &[Record]) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut map = BTreeMap::new();
    for (_, ops) in records {
        for (key, value) in ops {
            match value {
                Some(value) => map.insert(key.clone(), value.clone()),
                None => map.remove(key),
            };
        }
    }
    map
}

fn contents(tree: &Tree) -> BTreeMap<Vec<u8>, Vec<u8>> {
    tree.iter().map(Result::unwrap).collect()
}

fn run(config: TreeConfig, seed: u64, rounds: usize) {
    let mut rng = Rng(seed | 1);
    let store = FaultStore::new(&config);
    let mut tree = Tree::create(config, Box::new(store.clone())).unwrap();
    let mut store = store;
    let mut log = LossyLog::default();
    for round in 0..rounds {
        let context = format!("seed {seed:#x} round {round}");
        let crash_at = rng.below(300);
        for step in 0..300 {
            if step == crash_at {
                match rng.below(4) {
                    0 => store.die_after(rng.below(80)),
                    1 => store.fail_next_commit(CommitFault::AfterCopy),
                    2 => log.0.lock().unwrap().fail_sync = true,
                    _ => {}
                }
            }
            let key = |rng: &mut Rng| (rng.below(300) as u32).to_be_bytes().to_vec();
            match rng.below(20) {
                0 => {
                    let _ = tree.checkpoint_with(|point| log.sync_to(point.captured_seq));
                }
                1..=4 => {
                    // A batch of up to four operations, logged then applied.
                    let keys: Vec<_> = (0..1 + rng.below(4)).map(|_| key(&mut rng)).collect();
                    let value = vec![step as u8; rng.below(17) as usize];
                    let ops: Vec<Op<'_>> = keys
                        .iter()
                        .enumerate()
                        .map(|(i, key)| {
                            if i == 2 {
                                Op::Delete { key }
                            } else {
                                Op::Put { key, value: &value }
                            }
                        })
                        .collect();
                    let seq = log.append_batch(&ops);
                    let _ = tree.apply(Entry { seq, ops: &ops });
                }
                5..=6 => {
                    let _ = tree.remove_logged(&log, &key(&mut rng));
                }
                _ => {
                    let value = vec![step as u8; rng.below(17) as usize];
                    let _ = tree.put_logged(&log, &key(&mut rng), &value);
                }
            }
            if step > crash_at && (tree.store_failed() || rng.below(6) == 0) {
                break;
            }
        }

        // Crash the store and the log, and recover.
        drop(tree);
        let survivors = log.crash(&mut rng);
        let (recovered, recovered_store) = recover(store.crash(&mut rng), config, &mut rng);
        tree = recovered;
        store = recovered_store;
        let applied = tree.applied_seq();
        let last = survivors.last().map_or(0, |(seq, _)| *seq);
        assert!(applied <= last, "{context}: committed past the durable log");
        for record in survivors.iter().filter(|(seq, _)| *seq > applied) {
            apply_record(&tree, record).unwrap();
        }
        assert_eq!(contents(&tree), state_of(&survivors), "{context}");
        assert_eq!(tree.check_invariants(), Ok(()), "{context}");
        log = LossyLog::resume(survivors);
    }
}

#[test]
fn recovery_with_a_lossy_log_replays_to_the_durable_state() {
    let config = TreeConfig {
        circular_buffer_bytes: 1024,
        ..config(256, 128, 8, 16)
    };
    for seed in 0..24 {
        run(config, 0x10c + seed * 7919, 10);
    }
}

#[test]
fn recovery_with_a_lossy_log_and_a_large_buffer() {
    let config = TreeConfig {
        circular_buffer_bytes: 64 << 10,
        write_load_full_page: true,
        ..config(256, 128, 8, 16)
    };
    for seed in 0..12 {
        run(config, 0xb16 + seed * 104_729, 10);
    }
}

/// A follower opens the leader's snapshot and applies the log above its
/// `N`, and then equals the leader.
#[test]
fn a_follower_catches_up_from_a_snapshot_and_the_log() {
    let config = config(256, 128, 8, 16);
    let leader = Tree::in_memory(config).unwrap();
    let log = LossyLog::default();
    for i in 0..400u32 {
        let key = (i * 37 % 500).to_be_bytes();
        if i % 9 == 0 {
            leader.remove_logged(&log, &key).unwrap();
        } else {
            leader.put_logged(&log, &key, &[i as u8; 7]).unwrap();
        }
        if i % 100 == 99 {
            leader
                .checkpoint_with(|point| log.sync_to(point.captured_seq))
                .unwrap();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.bf");
    let mut file = std::fs::File::create(&path).unwrap();
    let state = leader.write_snapshot(&mut file).unwrap();
    drop(file);
    // The leader keeps writing after the snapshot.
    for i in 0..100u32 {
        leader.put_logged(&log, &i.to_be_bytes(), b"late").unwrap();
    }

    let follower = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    assert_eq!(follower.applied_seq(), state.applied_seq);
    let records = log.0.lock().unwrap().all.clone();
    for record in records.iter().filter(|(seq, _)| *seq > state.applied_seq) {
        apply_record(&follower, record).unwrap();
    }
    assert_eq!(contents(&follower), contents(&leader));
    assert_eq!(follower.check_invariants(), Ok(()));
}
