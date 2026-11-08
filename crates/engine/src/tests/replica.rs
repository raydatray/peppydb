//! A standalone leader and an external follower, end to end (plan 0022).
//!
//! The leader's WAL is the entry stream, read with `Wal::replay` as a log
//! shipper would. The leader keeps one large segment, so the stream holds
//! every entry from sequence 1.

use std::collections::BTreeMap;

use super::external::external;
use super::small;
use crate::*;

/// One entry of the stream, owned: its sequence and operations (`None`
/// value for a delete).
type Entry = (u64, Vec<(Vec<u8>, Option<Vec<u8>>)>);

fn leader_options() -> Options {
    Options {
        segment_bytes: 1 << 30,
        ..small()
    }
}

/// Every entry the leader's WAL holds. All the leader's writes were
/// acknowledged, so the WAL has no torn tail and the read changes nothing.
fn stream(leader: &Db) -> Vec<Entry> {
    let mut entries = Vec::new();
    engine_wal::Wal::replay(&leader.path().join("wal"), 1, |record| {
        let ops = record
            .ops
            .iter()
            .map(|op| match *op {
                engine_wal::Op::Put { key, value } => (key.to_vec(), Some(value.to_vec())),
                engine_wal::Op::Delete { key } => (key.to_vec(), None),
            })
            .collect();
        entries.push((record.seq, ops));
        Ok::<_, WalError>(())
    })
    .unwrap();
    entries
}

/// Apply `entry` to the follower.
fn ship(follower: &Db, (seq, ops): &Entry) -> Applied {
    let ops: Vec<Op<'_>> = ops
        .iter()
        .map(|(key, value)| match value {
            Some(value) => Op::Put { key, value },
            None => Op::Delete { key },
        })
        .collect();
    follower.apply(*seq, &ops).unwrap()
}

fn state_at(entries: &[Entry], seq: u64) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut map = BTreeMap::new();
    for (_, ops) in entries.iter().take_while(|(s, _)| *s <= seq) {
        for (key, value) in ops {
            match value {
                Some(value) => map.insert(key.clone(), value.clone()),
                None => map.remove(key),
            };
        }
    }
    map
}

fn contents(db: &Db) -> BTreeMap<Vec<u8>, Vec<u8>> {
    db.range(None, None).unwrap().map(Result::unwrap).collect()
}

/// A deterministic generator.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

/// Random acknowledged writes and batches on the leader.
fn write_some(leader: &Db, rng: &mut Rng, count: u64) {
    for _ in 0..count {
        let key = (rng.below(300) as u16).to_be_bytes();
        let value = vec![rng.below(256) as u8; 1 + rng.below(15) as usize];
        match rng.below(6) {
            0 => leader.delete(&key).unwrap(),
            1 => {
                let mut batch = WriteBatch::new();
                for _ in 0..1 + rng.below(4) {
                    batch.put(&(rng.below(300) as u16).to_be_bytes(), &value);
                }
                batch.delete(&key);
                leader.write(batch).unwrap();
            }
            _ => leader.put(&key, &value).unwrap(),
        }
    }
}

fn install(follower: Db, leader: &Db) -> Db {
    let mut bytes = Vec::new();
    leader.write_snapshot(&mut bytes).unwrap();
    follower.install_snapshot(&mut bytes.as_slice()).unwrap()
}

#[test]
fn a_follower_installs_a_snapshot_and_catches_up() {
    let leader_dir = tempfile::tempdir().unwrap();
    let leader = Db::open(leader_dir.path(), leader_options()).unwrap();
    let mut rng = Rng(0x1ead);
    write_some(&leader, &mut rng, 400);
    leader.checkpoint().unwrap();
    write_some(&leader, &mut rng, 200);

    let dir = tempfile::tempdir().unwrap();
    let follower = install(Db::open(dir.path(), external()).unwrap(), &leader);
    let from = follower.applied_seq();
    assert!(from > 0);
    for entry in stream(&leader).iter().filter(|(seq, _)| *seq > from) {
        assert_eq!(ship(&follower, entry), Applied::Applied);
    }
    assert_eq!(follower.applied_seq(), leader.applied_seq());
    assert_eq!(contents(&follower), contents(&leader));
}

#[test]
fn a_follower_that_crashes_resumes_and_converges() {
    let leader_dir = tempfile::tempdir().unwrap();
    let leader = Db::open(leader_dir.path(), leader_options()).unwrap();
    let mut rng = Rng(0xfa11);
    write_some(&leader, &mut rng, 300);
    let dir = tempfile::tempdir().unwrap();
    let follower = Db::open(dir.path(), external()).unwrap();
    for entry in &stream(&leader) {
        ship(&follower, entry);
    }
    follower.checkpoint().unwrap();
    let committed = follower.applied_seq();
    write_some(&leader, &mut rng, 300);
    for entry in stream(&leader).iter().filter(|(seq, _)| *seq > committed) {
        ship(&follower, entry);
    }
    drop(follower); // a crash: nothing since the checkpoint is kept

    let follower = Db::open(dir.path(), external()).unwrap();
    assert_eq!(follower.applied_seq(), committed);
    // The owner redelivers everything it still retains above the commit;
    // older entries are skipped.
    let entries = stream(&leader);
    for entry in &entries {
        let applied = ship(&follower, entry);
        assert_eq!(applied == Applied::AlreadyApplied, entry.0 <= committed);
    }
    assert_eq!(contents(&follower), contents(&leader));
}

/// The leader writes; the follower applies the stream with random
/// redeliveries, checkpoints, crashes, and snapshot reinstalls. After each
/// step the follower equals the leader's state at the follower's applied
/// sequence.
#[test]
fn a_randomized_follower_tracks_the_leader() {
    for seed in 0..4u64 {
        let mut rng = Rng(0xab5 + seed * 7919);
        let leader_dir = tempfile::tempdir().unwrap();
        let leader = Db::open(leader_dir.path(), leader_options()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut follower = Db::open(dir.path(), external()).unwrap();
        for step in 0..60 {
            let context = format!("seed {seed} step {step}");
            let count = 1 + rng.below(15);
            write_some(&leader, &mut rng, count);
            let entries = stream(&leader);
            match rng.below(10) {
                0 => {
                    leader.checkpoint().unwrap();
                    follower = install(follower, &leader);
                }
                1 => follower.checkpoint().map(drop).unwrap(),
                2 => {
                    drop(follower);
                    follower = Db::open(dir.path(), external()).unwrap();
                }
                _ => {}
            }
            // Ship a random amount, starting a little before the applied
            // sequence so some entries are redelivered.
            let applied = follower.applied_seq();
            let start = applied.saturating_sub(rng.below(5));
            let take = rng.below(entries.len() as u64 + 1) as usize;
            for entry in entries.iter().filter(|(seq, _)| *seq > start).take(take) {
                let expected = if entry.0 <= follower.applied_seq() {
                    Applied::AlreadyApplied
                } else {
                    Applied::Applied
                };
                assert_eq!(ship(&follower, entry), expected, "{context}");
            }
            assert_eq!(
                contents(&follower),
                state_at(&entries, follower.applied_seq()),
                "{context}"
            );
        }
    }
}

#[test]
fn the_stream_helper_reads_the_whole_log() {
    let dir = tempfile::tempdir().unwrap();
    let leader = Db::open(dir.path(), leader_options()).unwrap();
    leader.put(b"a", b"1").unwrap();
    leader.put(b"b", b"2").unwrap();
    let entries = stream(&leader);
    assert_eq!(entries.iter().map(|e| e.0).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(state_at(&entries, 2), contents(&leader));
}
