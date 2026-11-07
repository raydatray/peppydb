//! Entries and writes that a log sequences (plan 0019).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::*;

fn small() -> TreeConfig {
    config(256, 128, 8, 16)
}

/// One appended record: its sequence, key, and value (`None` for a delete).
pub(super) type Record = (u64, Vec<u8>, Option<Vec<u8>>);

/// A log that hands out increasing sequences and records every append.
#[derive(Default)]
pub(super) struct RecordingLog {
    next: Mutex<u64>,
    pub(super) records: Mutex<Vec<Record>>,
    fail: AtomicBool,
}

impl RecordingLog {
    pub(super) fn starting_after(seq: u64) -> Self {
        Self {
            next: Mutex::new(seq),
            ..Self::default()
        }
    }
}

impl Log for RecordingLog {
    fn append(&self, op: Op<'_>) -> Result<u64, LogError> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(LogError::Other("injected"));
        }
        // One lock across numbering and recording: strictly increasing in
        // real time across callers, as the contract requires.
        let mut next = self.next.lock().unwrap();
        *next += 1;
        let value = match op {
            Op::Put { value, .. } => Some(value.to_vec()),
            Op::Delete { .. } => None,
        };
        self.records
            .lock()
            .unwrap()
            .push((*next, op.key().to_vec(), value));
        Ok(*next)
    }
}

fn put<'a>(key: &'a [u8], value: &'a [u8]) -> Op<'a> {
    Op::Put { key, value }
}

#[test]
fn applied_entries_match_the_oracle() {
    let tree = Tree::in_memory(small()).unwrap();
    let mut oracle = BTreeMap::new();
    let mut seq = 0;
    for round in 0..300u32 {
        let keys: Vec<[u8; 4]> = (0..3)
            .map(|i| (round * 7 + i * 131) % 500)
            .map(u32::to_be_bytes)
            .collect();
        let value = [round as u8; 9];
        let ops: Vec<Op<'_>> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| {
                if (round + i as u32).is_multiple_of(5) {
                    Op::Delete { key }
                } else {
                    put(key, &value)
                }
            })
            .collect();
        for op in &ops {
            match op {
                Op::Put { key, value } => oracle.insert(key.to_vec(), value.to_vec()),
                Op::Delete { key } => oracle.remove(*key),
            };
        }
        seq += 1 + u64::from(round % 3); // gaps are allowed
        tree.apply(Entry { seq, ops: &ops }).unwrap();
        assert_eq!(tree.applied_seq(), seq);
    }
    let all: BTreeMap<_, _> = tree.iter().map(Result::unwrap).collect();
    assert_eq!(all, oracle);
    let state = tree.checkpoint().unwrap();
    assert_eq!(state.applied_seq, seq);
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn an_invalid_op_rejects_the_whole_entry_unchanged() {
    let tree = Tree::in_memory(small()).unwrap();
    let long = [7u8; 9];
    let ops = [put(b"a", b"1"), put(&long, b"2")];
    assert_eq!(
        tree.apply(Entry { seq: 1, ops: &ops }),
        Err(TreeError::KeyTooLarge)
    );
    assert_eq!(tree.get(b"a"), Ok(None));
    assert_eq!(tree.applied_seq(), 0);
    let ops = [put(b"a", &[0; 17])];
    assert_eq!(
        tree.apply(Entry { seq: 1, ops: &ops }),
        Err(TreeError::ValueTooLarge)
    );
    assert_eq!(
        tree.check_op(&Op::Delete { key: b"" }),
        Err(TreeError::EmptyKey)
    );
}

#[test]
fn empty_and_multi_op_entries_advance_the_watermark_once() {
    let tree = Tree::in_memory(small()).unwrap();
    tree.apply(Entry { seq: 4, ops: &[] }).unwrap();
    assert_eq!(tree.applied_seq(), 4);
    let ops = [put(b"a", b"1"), put(b"b", b"2"), Op::Delete { key: b"a" }];
    tree.apply(Entry { seq: 9, ops: &ops }).unwrap();
    assert_eq!(tree.applied_seq(), 9);
    assert_eq!(tree.get(b"a"), Ok(None), "later ops in an entry win");
    assert_eq!(tree.get(b"b"), Ok(Some(b"2".to_vec())));
}

#[test]
fn logged_writes_take_the_log_sequence() {
    let tree = Tree::in_memory(small()).unwrap();
    let log = RecordingLog::starting_after(100);
    assert_eq!(tree.put_logged(&log, b"a", b"1"), Ok(101));
    assert_eq!(tree.remove_logged(&log, b"a"), Ok(102));
    assert_eq!(tree.put_logged(&log, b"b", b"2"), Ok(103));
    assert_eq!(tree.applied_seq(), 103);
    assert_eq!(tree.get(b"a"), Ok(None));
    assert_eq!(tree.get(b"b"), Ok(Some(b"2".to_vec())));
    assert_eq!(log.records.lock().unwrap().len(), 3);
}

#[test]
fn a_failing_append_applies_nothing() {
    let tree = Tree::in_memory(small()).unwrap();
    let log = RecordingLog::default();
    log.fail.store(true, Ordering::Relaxed);
    assert_eq!(
        tree.put_logged(&log, b"a", b"1"),
        Err(TreeError::Log(LogError::Other("injected")))
    );
    assert_eq!(tree.get(b"a"), Ok(None));
    assert_eq!(tree.applied_seq(), 0);
    assert_eq!(
        tree.put_logged(&log, b"a", &[0; 17]),
        Err(TreeError::ValueTooLarge)
    );
}

/// The log sequence is fixed under the leaf lock, so for one key the
/// higher sequence is the value left behind.
#[test]
fn concurrent_logged_writes_to_one_key_end_at_the_highest_sequence() {
    let tree = Tree::in_memory(small()).unwrap();
    let log = RecordingLog::default();
    let highest = AtomicU64::new(0);
    std::thread::scope(|scope| {
        for t in 0..4u8 {
            let (tree, log, highest) = (&tree, &log, &highest);
            scope.spawn(move || {
                for i in 0..500u32 {
                    let value = [t, (i >> 8) as u8, i as u8];
                    let seq = tree.put_logged(log, b"k", &value).unwrap();
                    highest.fetch_max(seq, Ordering::Relaxed);
                }
            });
        }
    });
    let records = log.records.lock().unwrap();
    let (last, _, value) = records.iter().max_by_key(|(seq, ..)| *seq).unwrap();
    assert_eq!(*last, highest.load(Ordering::Relaxed));
    assert_eq!(tree.get(b"k").unwrap(), *value);
}

#[test]
fn internal_and_external_sequences_cannot_be_mixed() {
    let tree = Tree::in_memory(small()).unwrap();
    tree.apply(Entry {
        seq: 1,
        ops: &[put(b"a", b"1")],
    })
    .unwrap();
    assert_eq!(tree.put(b"b", b"2"), Err(TreeError::MixedSequencing));
    assert_eq!(tree.remove(b"a"), Err(TreeError::MixedSequencing));
    assert_eq!(tree.get(b"b"), Ok(None), "nothing changed");
    // Logged writes are external too.
    let log = RecordingLog::starting_after(1);
    assert_eq!(tree.put_logged(&log, b"c", b"3"), Ok(2));

    let tree = Tree::in_memory(small()).unwrap();
    tree.put(b"a", b"1").unwrap();
    assert_eq!(
        tree.apply(Entry { seq: 5, ops: &[] }),
        Err(TreeError::MixedSequencing)
    );
    assert_eq!(
        tree.put_logged(&RecordingLog::default(), b"b", b"2"),
        Err(TreeError::MixedSequencing)
    );
}

#[test]
fn stale_entries_are_rejected() {
    let tree = Tree::in_memory(small()).unwrap();
    tree.apply(Entry {
        seq: 10,
        ops: &[put(b"a", b"new")],
    })
    .unwrap();
    for seq in [3, 10] {
        assert_eq!(
            tree.apply(Entry {
                seq,
                ops: &[put(b"a", b"old")]
            }),
            Err(TreeError::SequenceOrder { seq, applied: 10 })
        );
    }
    assert_eq!(tree.get(b"a"), Ok(Some(b"new".to_vec())));
}

#[test]
fn the_latch_resets_on_open_and_sequences_continue_from_the_commit() {
    let tree = Tree::in_memory(small()).unwrap();
    tree.apply(Entry {
        seq: 40,
        ops: &[put(b"a", b"1")],
    })
    .unwrap();
    assert_eq!(
        tree.last_seq.load(Ordering::Acquire),
        40,
        "last_seq follows it"
    );
    tree.checkpoint().unwrap();
    let tree = Tree::open(tree.into_store()).unwrap();
    // A store applied externally may continue with internal sequences.
    tree.put(b"b", b"2").unwrap();
    assert_eq!(tree.applied_seq(), 41);
    let state = tree.checkpoint().unwrap();
    assert_eq!(state.applied_seq, 41);
}

#[test]
fn a_failed_entry_can_be_retried() {
    use std::sync::Arc;
    // A store whose page writes fail: an entry fails while it applies.
    let config = TreeConfig {
        circular_buffer_bytes: 1024,
        on_store_failure: StoreFailurePolicy::AcceptWrites,
        ..small()
    };
    let fail = Arc::new(AtomicBool::new(false));
    let store = super::evict::Failing::new(&config, fail.clone());
    let tree = Tree::create(config, Box::new(store)).unwrap();
    let mut seq = 0;
    let value = [5u8; 16];
    fail.store(true, Ordering::Relaxed);
    let failed = loop {
        seq += 1;
        let key = (seq as u32).wrapping_mul(2_654_435_761).to_be_bytes();
        let ops = [put(&key, &value)];
        match tree.apply(Entry { seq, ops: &ops }) {
            Ok(()) => assert!(seq < 5000, "no write failed"),
            Err(TreeError::Store(_)) => break key,
            Err(error) => panic!("{error:?}"),
        }
    };
    assert_eq!(
        tree.applied_seq(),
        seq - 1,
        "the failed entry is not applied"
    );
    fail.store(false, Ordering::Relaxed);
    let ops = [put(&failed, &value)];
    tree.apply(Entry { seq, ops: &ops }).unwrap();
    assert_eq!(tree.applied_seq(), seq);
    assert_eq!(tree.get(&failed), Ok(Some(value.to_vec())));
}
