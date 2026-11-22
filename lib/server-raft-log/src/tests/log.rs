use std::sync::Barrier;

use super::disk::{dir, entry, pos, small};
use crate::{LogError, LogOptions, LogPos, RaftLog, SimFs, StoredVote};

type Log = RaftLog<SimFs>;

fn open(fs: &SimFs, options: LogOptions) -> Log {
    RaftLog::open(fs.clone(), dir(), options).unwrap().0
}

fn indexes(log: &Log) -> Vec<u64> {
    log.entries(..).unwrap().iter().map(|e| e.index).collect()
}

/// The entries that survive a crash with no torn bytes.
fn survivors(fs: &SimFs) -> Vec<u64> {
    let log = open(&SimFs::from_image(&fs.crash_clean()), small());
    indexes(&log)
}

#[test]
fn writes_are_readable_at_once_and_durable_after_flush() {
    let fs = SimFs::new();
    let log = open(&fs, small());
    let first = log.append(vec![entry(1, 1), entry(1, 2)]).unwrap();
    assert_eq!(first, 1);
    assert_eq!(indexes(&log), [1, 2]);
    assert_eq!(log.state().last, Some(pos(1, 2)));
    assert_eq!(log.durable(), 0);
    assert!(survivors(&fs).is_empty());

    assert_eq!(log.flush().unwrap(), 1);
    assert_eq!(log.durable(), 1);
    assert_eq!(survivors(&fs), [1, 2]);
    // An idle flush changes nothing.
    assert_eq!(log.flush().unwrap(), 1);
}

#[test]
fn consecutive_appends_share_one_sync() {
    let fs = SimFs::new();
    let log = open(&fs, LogOptions::default());
    log.append(vec![entry(1, 1)]).unwrap();
    log.flush().unwrap();
    let before = fs.ops();
    for i in 2..=11 {
        log.append(vec![entry(1, i)]).unwrap();
    }
    log.save_committed(Some(pos(1, 5))).unwrap();
    assert_eq!(log.flush().unwrap(), 12);
    assert_eq!(fs.ops() - before, 2, "one write and one sync");
}

#[test]
fn a_vote_ends_the_group_and_is_durable_with_every_earlier_write() {
    let fs = SimFs::new();
    let log = open(&fs, small());
    log.append(vec![entry(1, 1)]).unwrap();
    let vote = StoredVote {
        term: 2,
        node: Some(7),
        committed: false,
    };
    let id = log.save_vote(vote).unwrap();
    log.append(vec![entry(2, 2)]).unwrap();
    assert_eq!(log.state().vote, Some(vote));
    assert_eq!(log.flush().unwrap(), id + 1);
    let (restarted, recovered) =
        RaftLog::open(SimFs::from_image(&fs.crash_clean()), dir(), small()).unwrap();
    assert_eq!(recovered.vote, Some(vote));
    assert_eq!(indexes(&restarted), [1, 2]);
}

#[test]
fn purge_and_truncate_shape_the_log_state() {
    // As MemLog's test of the same name (plan 0028).
    let fs = SimFs::new();
    let log = open(&fs, small());
    log.append((1..=5).map(|i| entry(1, i)).collect()).unwrap();
    log.purge(pos(1, 2)).unwrap();
    log.truncate_after(Some(pos(1, 4))).unwrap();
    assert_eq!(indexes(&log), [3, 4]);
    assert!(survivors(&fs).is_empty(), "durable at the next flush");
    log.flush().unwrap();
    assert_eq!(survivors(&fs), [3, 4]);

    let state = log.state();
    assert_eq!(state.purged, Some(pos(1, 2)));
    assert_eq!(state.last, Some(pos(1, 4)));

    log.purge(pos(1, 4)).unwrap();
    assert_eq!(
        log.state().last,
        Some(pos(1, 4)),
        "the purge point when empty"
    );
    assert!(indexes(&log).is_empty());
    log.append(vec![entry(2, 5)]).unwrap();
    log.flush().unwrap();
    assert_eq!(survivors(&fs), [5]);
}

#[test]
fn durability_follows_the_write_order() {
    // Truncate after an append, then crash before the flush: the durable
    // log is the result of a prefix of the writes.
    let fs = SimFs::new();
    let log = open(&fs, small());
    log.append((1..=3).map(|i| entry(1, i)).collect()).unwrap();
    log.flush().unwrap();
    log.truncate_after(Some(pos(1, 1))).unwrap();
    log.append(vec![entry(2, 2)]).unwrap();
    log.purge(pos(1, 1)).unwrap();
    assert_eq!(indexes(&log), [2]);
    assert_eq!(log.entries(2..=2).unwrap(), [entry(2, 2)]);

    let restarted = open(&SimFs::from_image(&fs.crash_clean()), small());
    assert_eq!(
        indexes(&restarted),
        [1, 2, 3],
        "nothing after the crash point"
    );
    assert_eq!(restarted.entries(2..3).unwrap(), [entry(1, 2)]);
}

#[test]
fn appends_must_follow_the_last_entry() {
    let log = open(&SimFs::new(), small());
    log.append(vec![entry(1, 1)]).unwrap();
    assert!(matches!(
        log.append(vec![entry(1, 3)]),
        Err(LogError::NotContiguous {
            expected: 2,
            found: 3
        })
    ));
    assert!(matches!(
        log.append(vec![entry(1, 2), entry(1, 4)]),
        Err(LogError::NotContiguous { .. })
    ));
    // A refused append changes nothing and does not stop the log.
    assert_eq!(indexes(&log), [1]);
    log.append(vec![entry(1, 2)]).unwrap();
}

#[test]
fn reads_below_the_cache_come_from_the_segments() {
    let fs = SimFs::new();
    let tiny = LogOptions {
        cache_bytes: 300,
        ..small()
    };
    let log = open(&fs, tiny);
    for i in 1..=40 {
        log.append(vec![entry(1, i)]).unwrap();
        if i % 5 == 0 {
            log.flush().unwrap();
        }
    }
    // Unwritten entries stay cached whatever the budget.
    assert_eq!(indexes(&log), (1..=40).collect::<Vec<_>>());
    log.flush().unwrap();
    assert_eq!(
        log.entries(3..7).unwrap(),
        (3..7).map(|i| entry(1, i)).collect::<Vec<_>>()
    );
    assert_eq!(indexes(&log), (1..=40).collect::<Vec<_>>());
    // A truncation below the cache, then new entries.
    log.truncate_after(Some(pos(1, 10))).unwrap();
    log.append(vec![entry(2, 11)]).unwrap();
    assert_eq!(
        log.entries(9..).unwrap(),
        [entry(1, 9), entry(1, 10), entry(2, 11)]
    );
    log.flush().unwrap();
    assert_eq!(
        log.entries(9..).unwrap(),
        [entry(1, 9), entry(1, 10), entry(2, 11)]
    );
}

#[test]
fn a_failed_sync_fails_every_later_write_and_reports_nothing_durable() {
    let fs = SimFs::new();
    let log = open(&fs, small());
    log.append(vec![entry(1, 1)]).unwrap();
    log.flush().unwrap();
    let durable = log.durable();
    log.append(vec![entry(1, 2)]).unwrap();
    // The group's write succeeds; its sync fails.
    fs.fail_op(Some(fs.ops() + 1));
    let error = log.flush().unwrap_err();
    assert!(
        matches!(
            error,
            LogError::Io {
                op: "sync group",
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(log.durable(), durable);
    assert!(matches!(
        log.append(vec![entry(1, 3)]),
        Err(LogError::Failed { .. })
    ));
    assert!(matches!(
        log.save_vote(StoredVote {
            term: 1,
            node: None,
            committed: false
        }),
        Err(LogError::Failed { .. })
    ));
    assert!(matches!(log.flush(), Err(LogError::Failed { .. })));
    fs.fail_op(None);
    assert!(log.flush().is_err(), "the failure is sticky");
}

#[test]
fn writes_before_a_failure_in_the_same_flush_are_durable() {
    let fs = SimFs::new();
    let log = open(&fs, small());
    let appended = log.append(vec![entry(1, 1)]).unwrap();
    log.save_vote(StoredVote {
        term: 1,
        node: Some(1),
        committed: false,
    })
    .unwrap();
    // Create, append, sync, and sync the directory for the new segment;
    // then the vote's first operation fails.
    fs.fail_op(Some(fs.ops() + 4));
    assert!(log.flush().is_err());
    assert_eq!(log.durable(), appended);
}

#[test]
fn reopen_continues_the_log() {
    let fs = SimFs::new();
    let log = open(&fs, small());
    log.append((1..=20).map(|i| entry(1, i)).collect()).unwrap();
    log.save_committed(Some(pos(1, 18))).unwrap();
    log.purge(pos(1, 3)).unwrap();
    log.flush().unwrap();
    drop(log);
    let (log, recovered) = RaftLog::open(fs.clone(), dir(), small()).unwrap();
    assert_eq!(recovered.committed, Some(pos(1, 18)));
    let state = log.state();
    assert_eq!(state.last, Some(pos(1, 20)));
    assert_eq!(state.purged, Some(pos(1, 3)));
    log.append(vec![entry(2, 21)]).unwrap();
    log.flush().unwrap();
    assert_eq!(indexes(&log), (4..=21).collect::<Vec<_>>());
    let _: LogPos = state.last.unwrap();
}

#[test]
fn a_truncation_past_the_last_entry_removes_nothing() {
    let fs = SimFs::new();
    let log = open(&fs, small());
    log.append((0..=10).map(|i| entry(1, i)).collect()).unwrap();
    log.truncate_after(Some(pos(1, 11))).unwrap();
    assert_eq!(log.state().last, Some(pos(1, 10)));
    assert_eq!(indexes(&log), (0..=10).collect::<Vec<_>>());
    log.flush().unwrap();
    assert_eq!(survivors(&fs), (0..=10).collect::<Vec<_>>());
}

#[test]
fn durability_queries_follow_the_disk() {
    let fs = SimFs::new();
    let log = open(&fs, small());
    log.append((1..=4).map(|i| entry(1, i)).collect()).unwrap();
    let vote = StoredVote {
        term: 1,
        node: Some(2),
        committed: false,
    };
    log.save_vote(vote).unwrap();
    assert!(!log.is_durable(pos(1, 2)));
    assert_eq!(log.durable_vote(), None);
    log.flush().unwrap();
    assert!(log.is_durable(pos(1, 2)));
    assert!(!log.is_durable(pos(2, 2)), "another term at the index");
    assert_eq!(log.durable_vote(), Some(vote));
    log.purge(pos(1, 3)).unwrap();
    log.flush().unwrap();
    assert!(log.is_durable(pos(1, 1)), "at or below the purge point");
    assert!(!log.is_failed());
    fs.cut_power_at(Some(fs.ops()));
    log.append(vec![entry(1, 5)]).unwrap();
    assert!(log.flush().is_err());
    assert!(log.is_failed());
}

/// Two flushes race for one write: the one that finds the queue empty
/// must still report the write durable once the other has synced it.
#[test]
fn a_flush_that_finds_the_queue_taken_reports_the_other_flushs_write() {
    const ROUNDS: u64 = 20_000;
    let log = open(&SimFs::new(), LogOptions::default());
    let start = Barrier::new(3);
    let done = Barrier::new(3);
    std::thread::scope(|scope| {
        let flushers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let mut stale = Vec::new();
                    for _ in 0..ROUNDS {
                        start.wait();
                        let target = log.submitted();
                        let durable = log.flush().unwrap();
                        if durable < target {
                            stale.push((target, durable));
                        }
                        done.wait();
                    }
                    stale
                })
            })
            .collect();
        for index in 1..=ROUNDS {
            log.append(vec![entry(1, index)]).unwrap();
            start.wait();
            done.wait();
        }
        let stale: Vec<_> = flushers
            .into_iter()
            .flat_map(|flusher| flusher.join().unwrap())
            .collect();
        assert!(
            stale.is_empty(),
            "{} flushes reported an earlier write not durable, such as (submitted, durable) {:?}",
            stale.len(),
            stale.first()
        );
    });
}
