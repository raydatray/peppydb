//! Crashes that lose unsynced WAL bytes (plan 0021).
//!
//! `Db::crash` abandons the WAL: records that were appended but never
//! synced are lost, and a prefix of them can be written torn. Writes that
//! are applied but not durable come from calling the tree's logged writes
//! directly, without waiting, under a long flush interval.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use super::small;
use crate::*;

/// One logged change, owned: a key and a value, `None` for a delete.
type Change = (Vec<u8>, Option<Vec<u8>>);

/// Every record the test appended, with its sequence.
#[derive(Default)]
struct Log(Vec<(u64, Vec<Change>)>);

impl Log {
    fn state_at(&self, seq: u64) -> BTreeMap<Vec<u8>, Vec<u8>> {
        let mut map = BTreeMap::new();
        for (_, changes) in self.0.iter().take_while(|(s, _)| *s <= seq) {
            for (key, value) in changes {
                match value {
                    Some(value) => map.insert(key.clone(), value.clone()),
                    None => map.remove(key),
                };
            }
        }
        map
    }
}

/// WAL-before-data, observed after a crash and before the reopen: every
/// logged record at or below the tree's committed sequence, from the first
/// segment that is still on disk, is in the durable WAL. A commit whose
/// hook did not sync the WAL leaves such records missing.
fn assert_wal_covers_the_commit(dir: &Path, log: &Log, context: &str) {
    let store = bf_tree::FileStore::open(dir.join("tree.bf")).unwrap();
    let committed = bf_tree::PageStore::state(&store).applied_seq;
    drop(store);
    let wal_dir = dir.join("wal");
    let mut names: Vec<u64> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_str()?
                .strip_suffix(".wal")?
                .parse()
                .ok()
        })
        .collect();
    names.sort_unstable();
    let Some(&first) = names.first() else {
        assert_eq!(committed, 0, "{context}: no WAL but a committed write");
        return;
    };
    let mut durable = std::collections::BTreeSet::new();
    engine_wal::Wal::replay(&wal_dir, first, |record| {
        durable.insert(record.seq);
        Ok::<_, WalError>(())
    })
    .unwrap();
    for (seq, _) in &log.0 {
        if (first..=committed).contains(seq) {
            assert!(
                durable.contains(seq),
                "{context}: committed record {seq} is not in the durable WAL"
            );
        }
    }
}

fn contents(db: &Db) -> BTreeMap<Vec<u8>, Vec<u8>> {
    db.range(None, None).unwrap().map(Result::unwrap).collect()
}

fn slow() -> Options {
    Options {
        flush_interval: Duration::from_secs(3600),
        segment_bytes: 1 << 30,
        ..small()
    }
}

/// A put that is applied but not waited for: under a long flush interval
/// it stays unsynced.
fn unsynced_put(db: &Db, key: &[u8], value: &[u8]) -> u64 {
    db.shared
        .tree
        .put_logged(&WalLog(&db.shared.wal), key, value)
        .unwrap()
}

/// A put made durable explicitly, as `Db::put` would after its wait: under
/// the long flush interval `Db::put` itself would wait an hour.
fn synced_put(db: &Db, key: &[u8], value: &[u8]) {
    let seq = unsynced_put(db, key, value);
    db.shared.wal.sync_to(seq).unwrap();
}

/// Acknowledged writes survive a crash; unsynced ones are lost.
#[test]
fn acknowledged_writes_survive_and_unsynced_ones_are_lost() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), slow()).unwrap();
    for i in 0..20u8 {
        synced_put(&db, &[i], b"acked");
    }
    for i in 0..20u8 {
        unsynced_put(&db, &[i], b"lost");
    }
    // A reader sees the unsynced value: the documented guarantee boundary.
    assert_eq!(db.get(&[3]).unwrap(), Some(b"lost".to_vec()));
    db.crash(0);
    let db = Db::open(dir.path(), slow()).unwrap();
    for i in 0..20u8 {
        assert_eq!(db.get(&[i]).unwrap(), Some(b"acked".to_vec()), "key {i}");
    }
    assert_eq!(db.applied_seq(), 20);
}

/// A torn last record, cut at every byte, is repaired away.
#[test]
fn a_torn_record_at_every_byte_is_repaired_away() {
    // The encoded size of a put of a one-byte key and value: len and crc,
    // seq and kind, then key_len, value_len, key, and value.
    let record = 8 + 9 + 8 + 2;
    for written in 0..record {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), slow()).unwrap();
        synced_put(&db, b"a", b"1");
        unsynced_put(&db, b"b", b"2");
        db.crash(written);
        let db = Db::open(dir.path(), slow()).unwrap();
        assert_eq!(
            db.get(b"a").unwrap(),
            Some(b"1".to_vec()),
            "written {written}"
        );
        assert_eq!(db.get(b"b").unwrap(), None, "written {written}");
    }
}

/// A checkpoint's hook syncs the WAL through every effect it commits, so
/// after a crash the state is still a prefix of the durable WAL.
#[test]
fn a_checkpoint_makes_its_unsynced_effects_durable_first() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), slow()).unwrap();
    let mut log = Log::default();
    for i in 0..300u32 {
        let key = i.to_be_bytes();
        let seq = unsynced_put(&db, &key, b"u");
        log.0.push((seq, vec![(key.to_vec(), Some(b"u".to_vec()))]));
    }
    // An unsynced batch: logged, then applied, not waited for.
    let changes: Vec<Change> = (0..4u8).map(|i| (vec![0xb0, i], Some(vec![i]))).collect();
    let ops: Vec<_> = changes
        .iter()
        .map(|(key, value)| engine_wal::Op::Put {
            key,
            value: value.as_deref().unwrap(),
        })
        .collect();
    let seq = db.shared.wal.append_batch(&ops).unwrap();
    let tree_ops = tree_ops(&changes);
    db.shared
        .tree
        .apply(bf_tree::Entry {
            seq,
            ops: &tree_ops,
        })
        .unwrap();
    log.0.push((seq, changes));
    db.checkpoint().unwrap();
    db.crash(0);
    assert_wal_covers_the_commit(dir.path(), &log, "checkpoint then crash");

    let db = Db::open(dir.path(), slow()).unwrap();
    let applied = db.applied_seq();
    assert_eq!(applied, seq, "the checkpoint synced everything it captured");
    assert_eq!(contents(&db), log.state_at(applied));
}

/// Deterministic generator.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

/// Run one generation of random writes, batches, and checkpoints, some
/// acknowledged and some not, then crash. Returns the highest acknowledged
/// sequence.
fn generation(db: Db, rng: &mut Rng, log: &mut Log) -> u64 {
    let mut acked = 0;
    for _ in 0..rng.below(40) + 1 {
        let key = (rng.below(64) as u16).to_be_bytes().to_vec();
        let value = vec![rng.below(256) as u8; rng.below(9) as usize];
        match rng.below(10) {
            0 => {
                db.checkpoint().unwrap();
            }
            1..=2 => {
                let mut batch = WriteBatch::new();
                let mut changes = BTreeMap::new();
                for _ in 0..rng.below(4) + 1 {
                    let key = (rng.below(64) as u16).to_be_bytes().to_vec();
                    if rng.below(4) == 0 {
                        batch.delete(&key);
                        changes.insert(key, None);
                    } else {
                        batch.put(&key, &value);
                        changes.insert(key, Some(value.clone()));
                    }
                }
                db.write(batch).unwrap();
                let seq = db.applied_seq();
                log.0.push((seq, changes.into_iter().collect()));
                acked = seq;
            }
            3 => {
                let seq = unsynced_put(&db, &key, &value);
                log.0.push((seq, vec![(key, Some(value))]));
            }
            4 => {
                db.delete(&key).unwrap();
                let seq = db.applied_seq();
                log.0.push((seq, vec![(key, None)]));
                acked = seq;
            }
            _ => {
                db.put(&key, &value).unwrap();
                let seq = db.applied_seq();
                log.0.push((seq, vec![(key, Some(value))]));
                acked = seq;
            }
        }
    }
    let written = rng.below(60) as usize;
    db.crash(written);
    acked
}

fn reopen(dir: &Path) -> Db {
    Db::open(
        dir,
        Options {
            flush_interval: Duration::ZERO,
            checkpoint_wal_bytes: 4 << 10,
            segment_bytes: 4096,
            ..small()
        },
    )
    .unwrap()
}

/// The crash model: many generations, each crashing the database left by
/// the last. After every crash, every acknowledged write is present, and
/// the state is the log's state at the recovered sequence, so each batch
/// is whole and nothing the WAL lacks is present.
#[test]
fn many_crash_generations_keep_acknowledged_writes_and_whole_batches() {
    // 120 generations; each costs real fsyncs (about 0.1 s on a laptop).
    for seed in 0..3u64 {
        let dir = tempfile::tempdir().unwrap();
        let mut rng = Rng(0xc0ffee + seed * 7919);
        let mut log = Log::default();
        let mut acked = 0;
        for round in 0..40 {
            let context = format!("seed {seed} round {round}");
            if round > 0 {
                assert_wal_covers_the_commit(dir.path(), &log, &context);
            }
            let db = reopen(dir.path());
            let applied = db.applied_seq();
            let context = format!("seed {seed} round {round}");
            assert!(
                applied >= acked,
                "{context}: an acknowledged write was lost"
            );
            assert_eq!(contents(&db), log.state_at(applied), "{context}");
            // Records above the recovered sequence never became durable.
            log.0.retain(|(seq, _)| *seq <= applied);
            acked = generation(db, &mut rng, &mut log);
        }
    }
}

/// Four writer threads with batches, and checkpoints racing them, crash
/// at a random moment. Each thread's keys end at a prefix of its own
/// history at least as long as what it saw acknowledged.
#[test]
fn concurrent_writers_with_racing_checkpoints_recover_a_prefix() {
    for seed in 0..6u64 {
        let dir = tempfile::tempdir().unwrap();
        let options = Options {
            checkpoint_wal_bytes: 2 << 10,
            segment_bytes: 4096,
            ..small()
        };
        let db = Db::open(dir.path(), options).unwrap();
        // Per thread: its history of changes, and how many were acked.
        let histories: Vec<(Vec<Vec<Change>>, usize)> = std::thread::scope(|scope| {
            let db = &db;
            let handles: Vec<_> = (0..4u8)
                .map(|t| {
                    scope.spawn(move || {
                        let mut rng = Rng(seed * 31 + u64::from(t) + 1);
                        let mut history = Vec::new();
                        for n in 0..150u16 {
                            let tag = n.to_be_bytes();
                            let key = |i: u64| vec![t, i as u8];
                            if rng.below(3) == 0 {
                                let mut batch = WriteBatch::new();
                                let changes: Vec<Change> = (0..4)
                                    .map(|i| {
                                        batch.put(&key(i), &tag);
                                        (key(i), Some(tag.to_vec()))
                                    })
                                    .collect();
                                db.write(batch).unwrap();
                                history.push(changes);
                            } else {
                                let k = key(rng.below(4));
                                db.put(&k, &tag).unwrap();
                                history.push(vec![(k, Some(tag.to_vec()))]);
                            }
                        }
                        let acked = history.len();
                        // Writes that may or may not survive.
                        for n in 150..160u16 {
                            let k = vec![t, (n % 4) as u8];
                            let tag = n.to_be_bytes();
                            unsynced_put(db, &k, &tag);
                            history.push(vec![(k, Some(tag.to_vec()))]);
                        }
                        (history, acked)
                    })
                })
                .collect();
            scope.spawn(move || {
                for _ in 0..20 {
                    db.checkpoint().unwrap();
                }
            });
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        db.crash(17);

        let db = Db::open(dir.path(), options).unwrap();
        let found = contents(&db);
        for (t, (history, acked)) in histories.iter().enumerate() {
            let mine: BTreeMap<_, _> = found
                .iter()
                .filter(|(key, _)| key[0] == t as u8)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let prefix_matches = (*acked..=history.len()).any(|p| {
                let mut map = BTreeMap::new();
                for changes in &history[..p] {
                    for (key, value) in changes {
                        map.insert(key.clone(), value.clone().unwrap());
                    }
                }
                map == mine
            });
            assert!(prefix_matches, "seed {seed} thread {t}");
        }
    }
}

/// A garbage page appended to the tree file after a crash is released
/// on open (plan 0018) and does not change the state.
#[test]
fn a_garbage_tree_page_after_a_crash_is_released() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), small()).unwrap();
    for i in 0..100u8 {
        db.put(&[i], &[i; 4]).unwrap();
    }
    db.checkpoint().unwrap();
    db.put(b"after", b"x").unwrap();
    let expected = contents(&db);
    db.crash(0);
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join("tree.bf"))
        .unwrap();
    file.write_all(&vec![0x5a; 256]).unwrap();
    drop(file);
    let db = Db::open(dir.path(), small()).unwrap();
    assert_eq!(contents(&db), expected);
}
