//! Crash tests with the prefix oracle (plan 0029, "Testing").
//!
//! After a crash, recovery must return the effect of a prefix of the
//! submitted writes that contains every write reported durable. An append
//! counts as one write per entry: a torn group can keep some entries of
//! one append, which Raft allows. One exception is allowed by design: a
//! crash inside a truncation can leave a longer log than the truncation
//! asked for, cut somewhere above its point, never a hole.

use std::collections::BTreeMap;
use std::path::Path;

use super::disk::{dir, entry, pos, small};
use crate::{
    EntryKind, Fs, LogEntry, LogOptions, LogPos, RaftLog, RealFs, Recovered, SimFs, StoredVote,
    WriteId,
};

/// One write as the oracle sees it.
#[derive(Clone, Debug)]
enum Op {
    Entry(LogEntry),
    Truncate(Option<LogPos>),
    Purge(LogPos),
    Vote(StoredVote),
    Committed(Option<LogPos>),
}

/// A step of a scripted or random workload.
#[derive(Clone, Debug)]
enum Step {
    Append(Vec<LogEntry>),
    Truncate(Option<LogPos>),
    Purge(LogPos),
    Vote(StoredVote),
    Committed(Option<LogPos>),
    Flush,
}

/// What a log holds: the model's state, or what recovery returned.
#[derive(Clone, Debug, Default, PartialEq)]
struct Image {
    entries: BTreeMap<u64, LogEntry>,
    purged: Option<LogPos>,
    vote: Option<StoredVote>,
    committed: Option<LogPos>,
}

impl Image {
    fn apply(&mut self, op: &Op) {
        match op {
            Op::Entry(entry) => {
                self.entries.insert(entry.index, entry.clone());
            }
            Op::Truncate(after) => {
                let from = after.map_or(0, |after| after.index + 1);
                self.entries.split_off(&from);
            }
            Op::Purge(upto) => {
                self.purged = self.purged.max(Some(*upto));
                self.entries = self.entries.split_off(&(upto.index + 1));
            }
            Op::Vote(vote) => self.vote = Some(*vote),
            Op::Committed(committed) => self.committed = *committed,
        }
    }

    /// Whether `seen` is this image, up to the committed log id, which is
    /// a lower bound no lower than the purge point.
    fn matches(&self, seen: &Image) -> bool {
        self.entries == seen.entries
            && self.purged == seen.purged
            && self.vote == seen.vote
            && seen.committed >= self.purged
            && seen.committed <= self.committed.max(self.purged)
    }
}

/// Check the prefix oracle: `seen` is the effect of `ops[..j]` for some
/// `j` that keeps every op of a write up to `durable`.
fn check(ops: &[(WriteId, Op)], durable: WriteId, seen: &Image) -> Result<(), String> {
    let must = ops.iter().take_while(|(id, _)| *id <= durable).count();
    let mut model = Image::default();
    for (_, op) in &ops[..must] {
        model.apply(op);
    }
    for j in must..=ops.len() {
        if model.matches(seen) {
            return Ok(());
        }
        if let Some((_, Op::Truncate(after))) = ops.get(j) {
            // Cut inside the truncation: a longer log, never a hole.
            let keep = after.map_or(0, |after| after.index + 1);
            let below: Vec<_> = model.entries.range(..keep).collect();
            let seen_below: Vec<_> = seen.entries.range(..keep).collect();
            let prefix = seen
                .entries
                .iter()
                .all(|(index, entry)| model.entries.get(index) == Some(entry));
            let cut = Image {
                entries: seen.entries.clone(),
                ..model.clone()
            };
            if below == seen_below && prefix && cut.matches(seen) {
                return Ok(());
            }
        }
        if let Some((_, op)) = ops.get(j) {
            model.apply(op);
        }
    }
    Err(format!("not a prefix: durable {durable}, seen {seen:#?}"))
}

/// Run `steps` until the log fails. Returns the ops submitted, with their
/// write numbers, and the last write reported durable.
fn run<F: Fs>(log: &RaftLog<F>, steps: &[Step]) -> (Vec<(WriteId, Op)>, WriteId) {
    let mut ops = Vec::new();
    for step in steps {
        let result = match step.clone() {
            Step::Append(entries) => log.append(entries.clone()).map(|id| {
                ops.extend(entries.into_iter().map(|entry| (id, Op::Entry(entry))));
            }),
            Step::Truncate(after) => log
                .truncate_after(after)
                .map(|id| ops.push((id, Op::Truncate(after)))),
            Step::Purge(upto) => log.purge(upto).map(|id| ops.push((id, Op::Purge(upto)))),
            Step::Vote(vote) => log.save_vote(vote).map(|id| ops.push((id, Op::Vote(vote)))),
            Step::Committed(committed) => log
                .save_committed(committed)
                .map(|id| ops.push((id, Op::Committed(committed)))),
            Step::Flush => log.flush().map(drop),
        };
        if result.is_err() {
            break;
        }
    }
    (ops, log.durable())
}

/// What recovery returns from `fs`.
fn recover<F: Fs>(fs: F, dir: &Path) -> Image {
    let (log, recovered): (_, Recovered) = RaftLog::open(fs, dir, small()).unwrap();
    Image {
        entries: log
            .entries(..)
            .unwrap()
            .into_iter()
            .map(|entry| (entry.index, entry))
            .collect(),
        purged: recovered.purged,
        vote: recovered.vote,
        committed: recovered.committed,
    }
}

fn vote(term: u64, node: u64) -> StoredVote {
    StoredVote {
        term,
        node: Some(node),
        committed: term.is_multiple_of(2),
    }
}

fn appends(term: u64, indexes: std::ops::RangeInclusive<u64>) -> Step {
    Step::Append(indexes.map(|index| entry(term, index)).collect())
}

fn scripts() -> Vec<(&'static str, Vec<Step>)> {
    vec![
        (
            "appends across segments with commits and votes",
            vec![
                Step::Vote(vote(1, 1)),
                appends(1, 1..=3),
                Step::Committed(Some(pos(1, 2))),
                appends(1, 4..=6),
                Step::Flush,
                appends(1, 7..=12),
                Step::Committed(Some(pos(1, 9))),
                Step::Vote(vote(2, 2)),
                appends(2, 13..=14),
                Step::Flush,
            ],
        ),
        (
            "a truncation followed by appends",
            vec![
                appends(1, 1..=12),
                Step::Flush,
                Step::Truncate(Some(pos(1, 4))),
                appends(2, 5..=9),
                Step::Flush,
                Step::Truncate(Some(pos(2, 7))),
                appends(3, 8..=8),
                Step::Truncate(None),
                appends(4, 1..=2),
                Step::Flush,
            ],
        ),
        (
            "purges inside and past the log",
            vec![
                appends(1, 1..=12),
                Step::Committed(Some(pos(1, 10))),
                Step::Flush,
                Step::Purge(pos(1, 6)),
                appends(1, 13..=14),
                Step::Flush,
                Step::Purge(pos(2, 30)),
                appends(2, 31..=32),
                Step::Committed(Some(pos(2, 31))),
                Step::Flush,
            ],
        ),
    ]
}

/// Crash `steps` after every file-system operation in turn, with several
/// tears, and check the oracle. `report_early` makes the harness believe
/// every submitted write is durable: the negative control.
fn crash_everywhere(steps: &[Step], report_early: bool) -> Result<u64, String> {
    let total = {
        let fs = SimFs::new();
        let (log, _) = RaftLog::open(fs.clone(), dir(), small()).unwrap();
        run(&log, steps);
        fs.ops()
    };
    let mut checked = 0;
    for cut in 0..=total {
        let fs = SimFs::new();
        fs.cut_power_at(Some(cut));
        let Ok((log, _)) = RaftLog::open(fs.clone(), dir(), small()) else {
            continue;
        };
        let (ops, mut durable) = run(&log, steps);
        if report_early {
            durable = log.submitted();
        }
        for seed in 0..6 {
            let seen = recover(SimFs::from_image(&fs.crash(seed * 7919 + cut)), dir());
            check(&ops, durable, &seen)
                .map_err(|error| format!("cut {cut} seed {seed}: {error}"))?;
            checked += 1;
        }
    }
    Ok(checked)
}

#[test]
fn every_crash_point_of_the_scripts_recovers_a_valid_prefix() {
    for (name, steps) in scripts() {
        let checked =
            crash_everywhere(&steps, false).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(checked > 100, "{name}: {checked} crashes checked");
    }
}

#[test]
fn the_oracle_rejects_a_log_that_reports_durability_early() {
    for (name, steps) in scripts() {
        assert!(
            crash_everywhere(&steps, true).is_err(),
            "{name}: the negative control passed"
        );
    }
}

/// A small deterministic stream for workloads.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((u128::from(z ^ (z >> 31)) * u128::from(n)) >> 64) as u64
    }
}

/// A random workload that uses the log as Raft does: contiguous appends,
/// truncations above the purge point, purges, rising votes and commits.
fn workload(rng: &mut Rng, len: usize) -> Vec<Step> {
    let mut model = Image::default();
    let mut term = 1;
    let mut steps = Vec::new();
    while steps.len() < len {
        let last = model
            .entries
            .keys()
            .next_back()
            .copied()
            .or(model.purged.map(|p| p.index));
        let next = last.map_or(1, |last| last + 1);
        let floor = model.purged.map_or(0, |p| p.index);
        let step = match rng.below(10) {
            0..=4 => {
                let count = 1 + rng.below(4);
                Step::Append((next..next + count).map(|i| entry(term, i)).collect())
            }
            5 => Step::Flush,
            6 if last.is_some_and(|last| last > floor) => {
                let after = floor + rng.below(last.unwrap() - floor + 1);
                term += 1;
                let pos = model
                    .entries
                    .get(&after)
                    .map(LogEntry::pos)
                    .or(model.purged.filter(|p| p.index == after));
                match pos {
                    Some(pos) => Step::Truncate(Some(pos)),
                    None => continue,
                }
            }
            7 if last.is_some() => {
                // Above the purge point; past the end, at the current term.
                let upto = floor + 1 + rng.below(last.unwrap() - floor + 2);
                let term = model.entries.get(&upto).map_or(term, |e| e.term);
                Step::Purge(pos(term, upto))
            }
            8 => {
                term += 1;
                Step::Vote(vote(term, rng.below(3)))
            }
            9 => match model.entries.values().next_back() {
                Some(entry) if Some(entry.pos()) >= model.committed => {
                    Step::Committed(Some(entry.pos()))
                }
                _ => Step::Flush,
            },
            _ => Step::Flush,
        };
        match &step {
            Step::Append(entries) => entries
                .iter()
                .for_each(|e| model.apply(&Op::Entry(e.clone()))),
            Step::Truncate(after) => model.apply(&Op::Truncate(*after)),
            Step::Purge(upto) => model.apply(&Op::Purge(*upto)),
            Step::Vote(vote) => model.apply(&Op::Vote(*vote)),
            Step::Committed(committed) => model.apply(&Op::Committed(*committed)),
            Step::Flush => {}
        }
        steps.push(step);
    }
    steps.push(Step::Flush);
    steps
}

#[test]
fn random_workloads_crashed_at_random_points_recover_a_valid_prefix() {
    for seed in 0..2_000 {
        let mut rng = Rng(seed);
        let steps = workload(&mut rng, 40);
        let fs = SimFs::new();
        let total = {
            let probe = SimFs::new();
            let (log, _) = RaftLog::open(probe.clone(), dir(), small()).unwrap();
            run(&log, &steps);
            probe.ops()
        };
        fs.cut_power_at(Some(rng.below(total + 1)));
        let (log, _) = RaftLog::open(fs.clone(), dir(), small()).unwrap();
        let (ops, durable) = run(&log, &steps);
        let seen = recover(SimFs::from_image(&fs.crash(seed)), dir());
        check(&ops, durable, &seen).unwrap_or_else(|error| panic!("seed {seed}: {error}"));
    }
}

/// Process crashes on real files: written bytes survive whether synced or
/// not, so a crash can only cut the last write. Cut it at every byte.
#[test]
fn a_write_cut_at_every_byte_on_real_files_is_repaired_away() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("log");
    let options = LogOptions::default();
    let (log, _) = RaftLog::open(RealFs, &base, options).unwrap();
    log.append((1..=3).map(|i| entry(1, i)).collect()).unwrap();
    log.flush().unwrap();
    let segment = base.join("00000000000000000001.log");
    let before = std::fs::metadata(&segment).unwrap().len();
    let last = LogEntry {
        kind: EntryKind::Membership,
        ..entry(1, 4)
    };
    log.append(vec![last.clone()]).unwrap();
    log.flush().unwrap();
    let after = std::fs::metadata(&segment).unwrap().len();
    drop(log);
    let full = std::fs::read(&segment).unwrap();

    for cut in before..=after {
        let copy = tmp.path().join(format!("cut-{cut}"));
        std::fs::create_dir(&copy).unwrap();
        std::fs::write(copy.join("00000000000000000001.log"), &full[..cut as usize]).unwrap();
        let (log, recovered) = RaftLog::open(RealFs, &copy, options).unwrap();
        let entries = log.entries(..).unwrap();
        if cut == after {
            assert_eq!(entries.last(), Some(&last));
            assert_eq!(recovered.repaired_bytes, 0);
        } else {
            assert_eq!(entries.len(), 3, "cut {cut}");
            assert_eq!(recovered.repaired_bytes, cut - before);
        }
        assert_eq!(
            std::fs::metadata(copy.join("00000000000000000001.log"))
                .unwrap()
                .len(),
            if cut == after { after } else { before }
        );
    }
}
