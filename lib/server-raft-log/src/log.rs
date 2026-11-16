//! `RaftLog`: the readable view, the entry cache, and the ordered write
//! queue (plan 0029, "The API").
//!
//! Every write is applied to the readable view at once and joins one
//! queue. `flush`, called by a driver, takes the whole queue and makes it
//! durable in order: consecutive appends and commit points share one
//! sync, and a truncation, a purge, or a vote ends the group. A write is
//! never durable while an earlier one is lost. `RaftLog` starts no thread.
//!
//! The entry cache holds the newest entries, contiguous up to the last
//! one, including every entry not yet written. Reads below it go to the
//! segments: those entries are on disk and unchanged by queued writes.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::ops::{Bound, RangeBounds};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::disk::Disk;
use super::format::MAX_PAYLOAD_BYTES;
use super::fs::Fs;
use super::{LogEntry, LogError, LogOptions, LogPos, Recovered, StoredVote};

/// A write's number in the queue, from 1. Write `n` is durable once
/// [`RaftLog::durable`] is at least `n`.
pub type WriteId = u64;

/// The readable view's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogState {
    pub purged: Option<LogPos>,
    /// The last entry, or `purged` if none remain.
    pub last: Option<LogPos>,
    pub vote: Option<StoredVote>,
    pub committed: Option<LogPos>,
}

/// One queued write.
#[derive(Debug)]
enum Write {
    Append(Vec<LogEntry>),
    Truncate(Option<LogPos>),
    Purge(LogPos),
    Vote(StoredVote),
    Committed(Option<LogPos>),
}

/// The bytes an entry costs the cache.
fn cost(entry: &LogEntry) -> usize {
    entry.payload.len() + 64
}

struct Cached {
    entry: LogEntry,
    /// The write that appended it.
    write: WriteId,
}

struct View {
    state: LogState,
    /// The last entry; `state.last` falls back to the purge point.
    last_entry: Option<LogPos>,
    /// The newest entries, contiguous, ending at `last_entry`.
    cache: VecDeque<Cached>,
    cache_bytes: usize,
    queue: Vec<(WriteId, Write)>,
    next_write: WriteId,
    durable: WriteId,
    failed: Option<(&'static str, ErrorKind)>,
}

impl View {
    fn check(&self) -> Result<(), LogError> {
        match self.failed {
            Some((op, kind)) => Err(LogError::Failed { op, kind }),
            None => Ok(()),
        }
    }

    /// The index the next appended entry must have, if known.
    fn next_index(&self) -> Option<u64> {
        self.last_entry
            .or(self.state.purged)
            .map(|pos| pos.index + 1)
    }

    /// The index of the first cached entry (the next index if none).
    fn cache_first(&self) -> u64 {
        self.next_index().unwrap_or(0) - self.cache.len() as u64
    }

    fn update_last(&mut self) {
        self.state.last = self.last_entry.or(self.state.purged);
    }

    fn submit_inner(&mut self, write: Write) -> WriteId {
        let id = self.next_write;
        self.next_write += 1;
        match &write {
            Write::Append(entries) => {
                for entry in entries {
                    self.cache_bytes += cost(entry);
                    self.cache.push_back(Cached {
                        entry: entry.clone(),
                        write: id,
                    });
                    self.last_entry = Some(entry.pos());
                }
            }
            Write::Truncate(after) => {
                let keep = after.map(|pos| pos.index);
                while let Some(back) = self.cache.back()
                    && keep.is_none_or(|keep| back.entry.index > keep)
                {
                    self.cache_bytes -= cost(&back.entry);
                    self.cache.pop_back();
                }
                // Truncating at or past the last entry removes nothing.
                if keep.is_none_or(|keep| self.last_entry.is_some_and(|last| keep < last.index)) {
                    self.last_entry = after.filter(|after| Some(*after) > self.state.purged);
                }
            }
            Write::Purge(upto) => {
                self.state.purged = self.state.purged.max(Some(*upto));
                while let Some(front) = self.cache.front()
                    && front.entry.index <= upto.index
                {
                    self.cache_bytes -= cost(&front.entry);
                    self.cache.pop_front();
                }
                if self.last_entry.is_some_and(|last| last.index <= upto.index) {
                    self.last_entry = None;
                }
            }
            Write::Vote(vote) => self.state.vote = Some(*vote),
            Write::Committed(committed) => self.state.committed = *committed,
        }
        self.update_last();
        self.queue.push((id, write));
        id
    }

    /// Drop the oldest entries over budget, if they are durable.
    fn evict(&mut self, budget: usize) {
        while self.cache_bytes > budget
            && let Some(front) = self.cache.front()
            && front.write <= self.durable
        {
            self.cache_bytes -= cost(&front.entry);
            self.cache.pop_front();
        }
    }
}

/// The on-disk Raft log over the file system `F`.
pub struct RaftLog<F: Fs> {
    view: Mutex<View>,
    disk: Mutex<Disk<F>>,
    options: LogOptions,
    /// Flushes that wrote something, for measurements.
    flushes: AtomicU64,
}

impl<F: Fs> RaftLog<F> {
    /// Open the log in `dir`, creating it if absent, and recover it.
    pub fn open(fs: F, dir: &Path, options: LogOptions) -> Result<(Self, Recovered), LogError> {
        let (disk, recovered) = Disk::open(fs, dir, options)?;
        let mut view = View {
            state: LogState {
                purged: recovered.purged,
                last: None,
                vote: recovered.vote,
                committed: recovered.committed,
            },
            last_entry: disk.last(),
            cache: VecDeque::new(),
            cache_bytes: 0,
            queue: Vec::new(),
            next_write: 1,
            durable: 0,
            failed: None,
        };
        view.update_last();
        let log = Self {
            view: Mutex::new(view),
            disk: Mutex::new(disk),
            options,
            flushes: AtomicU64::new(0),
        };
        Ok((log, recovered))
    }

    fn view(&self) -> MutexGuard<'_, View> {
        self.view.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn disk(&self) -> MutexGuard<'_, Disk<F>> {
        self.disk.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Append entries after the last one (or after the purge point).
    pub fn append(&self, entries: Vec<LogEntry>) -> Result<WriteId, LogError> {
        let mut view = self.view();
        view.check()?;
        let mut expected = view.next_index();
        for entry in &entries {
            if let Some(expected) = expected
                && entry.index != expected
            {
                return Err(LogError::NotContiguous {
                    expected,
                    found: entry.index,
                });
            }
            if entry.payload.len() > MAX_PAYLOAD_BYTES {
                return Err(LogError::EntryTooLarge {
                    bytes: entry.payload.len(),
                });
            }
            expected = Some(entry.index + 1);
        }
        Ok(view.submit_with(Write::Append(entries), self.options.cache_bytes))
    }

    /// Remove every entry after `after` (every entry for `None`).
    pub fn truncate_after(&self, after: Option<LogPos>) -> Result<WriteId, LogError> {
        self.submit(Write::Truncate(after))
    }

    /// Remove every entry at or below `upto` and record it as the purge
    /// point.
    pub fn purge(&self, upto: LogPos) -> Result<WriteId, LogError> {
        self.submit(Write::Purge(upto))
    }

    pub fn save_vote(&self, vote: StoredVote) -> Result<WriteId, LogError> {
        self.submit(Write::Vote(vote))
    }

    /// Record the committed log id. It joins the next group commit; losing
    /// the newest one in a crash is allowed, since it is a lower bound.
    pub fn save_committed(&self, committed: Option<LogPos>) -> Result<WriteId, LogError> {
        self.submit(Write::Committed(committed))
    }

    fn submit(&self, write: Write) -> Result<WriteId, LogError> {
        let mut view = self.view();
        view.check()?;
        Ok(view.submit_with(write, self.options.cache_bytes))
    }

    /// Make every queued write durable, in order. Returns the last write
    /// now durable. A failure is sticky: this and every later write and
    /// flush fail, and nothing after the last successful sync is reported
    /// durable.
    pub fn flush(&self) -> Result<WriteId, LogError> {
        let mut disk = self.disk();
        let queue = {
            let mut view = self.view();
            view.check()?;
            std::mem::take(&mut view.queue)
        };
        if !queue.is_empty() {
            self.flushes.fetch_add(1, Ordering::Relaxed);
        }
        let mut durable = None;
        let result = Self::write(&mut disk, queue, &mut durable);
        drop(disk);
        let mut view = self.view();
        if let Some(durable) = durable {
            view.durable = view.durable.max(durable);
        }
        view.evict(self.options.cache_bytes);
        match result {
            Ok(()) => Ok(view.durable),
            Err(error) => {
                view.failed = Some(match &error {
                    LogError::Io { op, source } => (*op, source.kind()),
                    _ => ("flush", ErrorKind::Other),
                });
                Err(error)
            }
        }
    }

    /// Perform `queue` on the disk, recording in `durable` the last write
    /// made durable.
    fn write(
        disk: &mut Disk<F>,
        queue: Vec<(WriteId, Write)>,
        durable: &mut Option<WriteId>,
    ) -> Result<(), LogError> {
        // The last write in the group being built.
        let mut grouped = None;
        for (id, write) in queue {
            match write {
                Write::Append(entries) => {
                    entries
                        .iter()
                        .try_for_each(|entry| disk.push_entry(entry))?;
                    grouped = Some(id);
                    continue;
                }
                Write::Committed(committed) => {
                    if let Some(committed) = committed {
                        disk.push_commit(committed)?;
                    }
                    grouped = Some(id);
                    continue;
                }
                _ => {}
            }
            // A truncation, a purge, or a vote ends the group.
            disk.sync_group()?;
            if let Some(grouped) = grouped.take() {
                *durable = Some(grouped);
            }
            match write {
                Write::Truncate(after) => disk.truncate_after(after.map(|pos| pos.index))?,
                Write::Purge(upto) => disk.purge(upto)?,
                Write::Vote(vote) => disk.save_vote(vote)?,
                Write::Append(_) | Write::Committed(_) => unreachable!("handled above"),
            }
            *durable = Some(id);
        }
        disk.sync_group()?;
        if let Some(grouped) = grouped {
            *durable = Some(grouped);
        }
        Ok(())
    }

    /// Flushes that had writes to make durable: a measure of group
    /// commit, as appends per flush.
    pub fn flushes(&self) -> u64 {
        self.flushes.load(Ordering::Relaxed)
    }

    /// The last write that is durable; 0 before any.
    pub fn durable(&self) -> WriteId {
        self.view().durable
    }

    /// The last write submitted; 0 before any.
    pub fn submitted(&self) -> WriteId {
        self.view().next_write - 1
    }

    /// The first cached index: reads from it on are served from memory
    /// and never touch the file system.
    pub fn cached_from(&self) -> u64 {
        self.view().cache_first()
    }

    /// Whether the entry at `pos` is durable: synced, or at or below the
    /// durable purge point.
    pub fn is_durable(&self, pos: LogPos) -> bool {
        let disk = self.disk();
        disk.term_at(pos.index) == Some(pos.term)
            || disk
                .state()
                .purged
                .is_some_and(|purged| purged.index >= pos.index)
    }

    /// The durable vote.
    pub fn durable_vote(&self) -> Option<StoredVote> {
        self.disk().state().vote
    }

    /// Whether a write or sync failed, so the log refuses writes.
    pub fn is_failed(&self) -> bool {
        self.view().failed.is_some()
    }

    /// The readable view's state.
    pub fn state(&self) -> LogState {
        self.view().state
    }

    /// The entries in `range` that the readable view holds, in order. A
    /// read stops at the first entry it does not find.
    pub fn entries(&self, range: impl RangeBounds<u64>) -> Result<Vec<LogEntry>, LogError> {
        let (from_disk, cached) = {
            let view = self.view();
            let Some(next) = view.next_index() else {
                return Ok(Vec::new());
            };
            let start = match range.start_bound() {
                Bound::Included(start) => *start,
                Bound::Excluded(start) => start.saturating_add(1),
                Bound::Unbounded => 0,
            };
            let end = match range.end_bound() {
                Bound::Included(end) => end.saturating_add(1),
                Bound::Excluded(end) => *end,
                Bound::Unbounded => u64::MAX,
            }
            .min(next);
            let start = start.max(view.state.purged.map_or(0, |purged| purged.index + 1));
            if start >= end {
                return Ok(Vec::new());
            }
            let cache_first = view.cache_first();
            let cached: Vec<_> = view
                .cache
                .iter()
                .map(|cached| &cached.entry)
                .filter(|entry| (start..end).contains(&entry.index))
                .cloned()
                .collect();
            (start..end.min(cache_first), cached)
        };
        let mut entries = Vec::new();
        if !from_disk.is_empty() {
            let disk = self.disk();
            for index in from_disk {
                match disk.read(index)? {
                    Some(entry) => entries.push(entry),
                    None if entries.is_empty() => continue,
                    None => return Ok(entries),
                }
            }
        }
        entries.extend(cached);
        Ok(entries)
    }
}

impl View {
    fn submit_with(&mut self, write: Write, budget: usize) -> WriteId {
        let id = self.submit_inner(write);
        self.evict(budget);
        id
    }
}
