//! Snapshots of the committed image while the tree runs (plan 0019).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::apply::RecordingLog;
use super::*;

fn small() -> TreeConfig {
    TreeConfig {
        circular_buffer_bytes: 2048,
        ..config(256, 128, 8, 16)
    }
}

fn contents(tree: &Tree) -> BTreeMap<Vec<u8>, Vec<u8>> {
    tree.iter().map(Result::unwrap).collect()
}

/// Open snapshot bytes as a tree.
fn open_snapshot(bytes: &[u8]) -> (Tree, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.bf");
    std::fs::write(&path, bytes).unwrap();
    let tree = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    (tree, dir)
}

#[test]
fn a_quiet_snapshot_equals_the_committed_tree() {
    let tree = Tree::in_memory(small()).unwrap();
    for i in 0..300u32 {
        tree.put(&i.to_be_bytes(), &[i as u8; 9]).unwrap();
    }
    let committed = tree.checkpoint().unwrap();
    // Uncommitted writes are not in the snapshot.
    tree.put(b"late", b"x").unwrap();
    let mut out = Vec::new();
    assert_eq!(tree.write_snapshot(&mut out), Ok(committed));
    let leaves = tree.leaves.page_table().into_iter().flatten().count();
    assert_eq!(out.len(), 8192 + leaves * 256, "dense: one page per leaf");
    let (copy, _dir) = open_snapshot(&out);
    assert_eq!(copy.committed(), committed);
    assert_eq!(copy.applied_seq(), committed.applied_seq);
    assert_eq!(copy.get(b"late"), Ok(None));
    assert_eq!(contents(&copy).len(), 300);
    assert_eq!(copy.check_invariants(), Ok(()));
}

/// During a write storm with a checkpoint thread, a snapshot opens at the
/// state it returned and equals the leader as of that commit; replaying
/// the log above its `N` makes it equal the leader.
#[test]
fn a_snapshot_during_a_write_storm_is_a_consistent_replica() {
    super::concurrent::with_watchdog(Duration::from_secs(60), || {
        let leader = Tree::in_memory(small()).unwrap();
        let log = RecordingLog::default();
        let stop = AtomicBool::new(false);
        let snapshots: Vec<_> = std::thread::scope(|scope| {
            let (leader, log, stop) = (&leader, &log, &stop);
            for w in 0..3u8 {
                scope.spawn(move || {
                    let mut n = 0u32;
                    while !stop.load(Ordering::Relaxed) {
                        let key = [w, ((n % 700) >> 8) as u8, (n % 700) as u8];
                        let mut value = vec![w];
                        value.extend(n.to_be_bytes());
                        if n.is_multiple_of(7) {
                            leader.remove_logged(log, &key).unwrap();
                        } else {
                            leader.put_logged(log, &key, &value).unwrap();
                        }
                        n += 1;
                    }
                });
            }
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    leader.checkpoint().unwrap();
                }
            });
            // Stop the other threads even if a snapshot fails.
            let _stopping = super::concurrent::StopOnDrop(stop);
            (0..5)
                .map(|_| {
                    std::thread::sleep(Duration::from_millis(15));
                    let mut out = Vec::new();
                    let state = leader.write_snapshot(&mut out).unwrap();
                    (state, out)
                })
                .collect()
        });

        let records = log.records.lock().unwrap().clone();
        let mut records = records;
        records.sort_by_key(|(seq, ..)| *seq);
        let final_state = contents(&leader);
        for (state, bytes) in snapshots {
            let (copy, _dir) = open_snapshot(&bytes);
            assert_eq!(copy.committed(), state);
            assert_eq!(copy.check_invariants(), Ok(()));
            // Replay every record above N, as a follower would.
            for (seq, key, value) in records.iter().filter(|(seq, ..)| *seq > state.applied_seq) {
                let ops = [match value {
                    Some(value) => Op::Put { key, value },
                    None => Op::Delete { key },
                }];
                copy.apply(Entry {
                    seq: *seq,
                    ops: &ops,
                })
                .unwrap();
            }
            assert_eq!(contents(&copy), final_state, "replica after replay");
        }
    });
}

#[test]
fn a_pin_defers_frees_until_a_commit_after_it_ends() {
    let tree = Tree::in_memory(small()).unwrap();
    let id = LeafId::new(0).unwrap();
    let mut bytes = tree.leaves.page_copy(id).unwrap();
    let pin = tree.leaves.pin_frees();
    tree.leaves.overwrite(id, &mut bytes).unwrap();
    let pages = tree.store().page_count();
    tree.checkpoint().unwrap();
    // The superseded page is still allocated: a new page grows the store.
    tree.leaves.overwrite(id, &mut bytes).unwrap();
    assert_eq!(tree.store().page_count(), pages + 1);
    drop(pin);
    tree.checkpoint().unwrap();
    // Both replaced pages are free now.
    tree.store().alloc().unwrap();
    tree.store().alloc().unwrap();
    assert_eq!(tree.store().page_count(), pages + 1);
}

#[test]
fn a_failing_output_reports_snapshot_output() {
    struct Broken;
    impl std::io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let tree = Tree::in_memory(small()).unwrap();
    assert_eq!(
        tree.write_snapshot(&mut Broken),
        Err(TreeError::SnapshotOutput {
            kind: std::io::ErrorKind::BrokenPipe,
            code: None,
        })
    );
}
