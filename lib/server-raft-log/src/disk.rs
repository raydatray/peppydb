//! The files: segments, the index of the entries they hold, the `state`
//! file, and recovery (plan 0029, "On-disk layout", "Truncation and
//! purge", "Recovery").
//!
//! `Disk` performs one write at a time; the queue in `log` orders them.
//! Appends and commit points accumulate in a group that `sync_group`
//! writes with one `sync_data`. Truncations, purges, and votes act at once
//! and must follow a `sync_group`.
//!
//! A segment is named by its first entry's index. Invariants:
//!
//! - Entries in a segment are contiguous and start at its name.
//! - A new segment starts only at a group boundary, when the active one
//!   holds at least `segment_bytes` and at least one entry; so only the
//!   last segment can hold no entry.
//! - `locs` lists every synced entry above the purge point, contiguous.

use std::collections::{BTreeMap, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use wal_segments::HEADER_BYTES;

use super::format::{self, FORMAT, Record, State};
use super::fs::{Fs, FsFile};
use super::{LogEntry, LogError, LogOptions, LogPos, Recovered, StoredVote, io};

const STATE: &str = "state";
const STATE_TMP: &str = "state.tmp";

/// Where an entry is.
#[derive(Clone, Copy, Debug)]
struct Loc {
    /// The segment's name.
    segment: u64,
    offset: u64,
    body_len: usize,
    term: u64,
}

pub(crate) struct Disk<F: Fs> {
    fs: F,
    dir: PathBuf,
    options: LogOptions,
    /// Open segments by name; the last is the active one.
    segments: BTreeMap<u64, F::File>,
    /// The index of `locs[0]`.
    first: u64,
    locs: VecDeque<Loc>,
    state: State,
    /// The group being built: its bytes, its entries, and their first
    /// index.
    buf: Vec<u8>,
    group: Vec<Loc>,
    group_first: u64,
    /// The active segment was created for this group, so the directory
    /// must be synced with it.
    new_segment: bool,
}

fn corrupt(file: &str, offset: usize, reason: &'static str) -> LogError {
    LogError::Corrupt {
        file: file.to_owned(),
        offset: offset as u64,
        reason,
    }
}

impl<F: Fs> Disk<F> {
    /// Open the log in `dir`, creating it if absent: read `state`, scan
    /// the segments, repair a torn tail in the last one, and finish an
    /// interrupted purge.
    pub(crate) fn open(
        fs: F,
        dir: &Path,
        options: LogOptions,
    ) -> Result<(Self, Recovered), LogError> {
        fs.create_dir_all(dir).map_err(io("create log directory"))?;
        let names = fs.list(dir).map_err(io("list log directory"))?;
        let mut sync_dir = false;
        let state = match names.iter().any(|name| name == STATE) {
            true => {
                let bytes = fs.read(&dir.join(STATE)).map_err(io("read state"))?;
                format::decode_state(&bytes).map_err(|reason| corrupt(STATE, 0, reason))?
            }
            false => State::default(),
        };
        if names.iter().any(|name| name == STATE_TMP) {
            // A state write that did not reach its rename.
            fs.remove(&dir.join(STATE_TMP))
                .map_err(io("remove state.tmp"))?;
            sync_dir = true;
        }
        let mut firsts: Vec<u64> = names
            .iter()
            .filter_map(|name| FORMAT.parse_name(name))
            .collect();
        firsts.sort_unstable();

        let keep_from = state.purged.map_or(0, |purged| purged.index + 1);
        let mut kept = Vec::new();
        let mut entries: Vec<(u64, Loc)> = Vec::new();
        let mut committed = None;
        let mut repaired_bytes = 0;
        for (at, first) in firsts.iter().copied().enumerate() {
            let last = at + 1 == firsts.len();
            let name = FORMAT.file_name(first);
            let path = dir.join(&name);
            let bytes = fs.read(&path).map_err(io("read segment"))?;
            match FORMAT.decode_header(&bytes) {
                Ok(named) if named == first => {}
                Ok(_) => return Err(corrupt(&name, 0, "header names another index")),
                // A crash while the last segment was created: it holds
                // nothing durable.
                Err(_) if last => {
                    fs.remove(&path).map_err(io("remove torn segment"))?;
                    sync_dir = true;
                    break;
                }
                Err(reason) => return Err(corrupt(&name, 0, reason)),
            }
            kept.push(first);
            // Wholly at or below the purge point: not read.
            if firsts.get(at + 1).is_some_and(|next| *next <= keep_from) {
                continue;
            }
            let mut offset = HEADER_BYTES;
            let mut previous = None;
            while offset < bytes.len() {
                let decoded = FORMAT
                    .decode_frame(&bytes[offset..])
                    .and_then(|(body, used)| Some((format::decode_body(body)?, body.len(), used)));
                let Some((record, body_len, used)) = decoded else {
                    if !last {
                        return Err(corrupt(&name, offset, "undecodable record"));
                    }
                    let after = entries.last().map(|(index, _)| *index);
                    let follows = FORMAT.intact_frame_after(&bytes, offset, |body| {
                        matches!(format::decode_body(body), Some(Record::Entry(entry))
                            if after.is_none_or(|after| entry.index > after))
                    });
                    if follows {
                        return Err(corrupt(&name, offset, "damage before intact records"));
                    }
                    let mut file = fs.open(&path).map_err(io("open segment"))?;
                    file.set_len(offset as u64).map_err(io("truncate tail"))?;
                    file.sync_data().map_err(io("sync truncated segment"))?;
                    repaired_bytes = (bytes.len() - offset) as u64;
                    break;
                };
                match record {
                    Record::Entry(entry) => {
                        let expected = previous.map_or(first, |previous: u64| previous + 1);
                        if entry.index != expected {
                            let reason = match previous {
                                None => "first entry does not match the segment name",
                                Some(_) => "index does not increase by one",
                            };
                            return Err(corrupt(&name, offset, reason));
                        }
                        previous = Some(entry.index);
                        let loc = Loc {
                            segment: first,
                            offset: offset as u64,
                            body_len,
                            term: entry.term,
                        };
                        entries.push((entry.index, loc));
                    }
                    Record::Commit(pos) => committed = Some(pos),
                }
                offset += used;
            }
        }

        entries.retain(|(index, _)| *index >= keep_from);
        for pair in entries.windows(2) {
            if pair[1].0 != pair[0].0 + 1 {
                let file = FORMAT.file_name(pair[1].1.segment);
                return Err(corrupt(
                    &file,
                    pair[1].1.offset as usize,
                    "entries are missing",
                ));
            }
        }
        if let (Some(_), Some((index, loc))) = (state.purged, entries.first())
            && *index != keep_from
        {
            let file = FORMAT.file_name(loc.segment);
            return Err(corrupt(
                &file,
                loc.offset as usize,
                "entries do not follow the purge point",
            ));
        }

        let mut segments = BTreeMap::new();
        for first in kept {
            let file = fs
                .open(&FORMAT.path(dir, first))
                .map_err(io("open segment"))?;
            segments.insert(first, file);
        }
        let mut disk = Disk {
            fs,
            dir: dir.to_path_buf(),
            options,
            segments,
            first: entries.first().map_or(keep_from, |(index, _)| *index),
            locs: entries.into_iter().map(|(_, loc)| loc).collect(),
            state,
            buf: Vec::new(),
            group: Vec::new(),
            group_first: 0,
            new_segment: false,
        };
        let removed = disk.remove_purged_segments()?;
        if sync_dir && !removed {
            disk.sync_dir()?;
        }
        let recovered = Recovered {
            vote: state.vote,
            purged: state.purged,
            last: disk.last().or(state.purged),
            committed: committed.max(state.purged),
            repaired_bytes,
        };
        Ok((disk, recovered))
    }

    fn sync_dir(&self) -> Result<(), LogError> {
        self.fs
            .sync_dir(&self.dir)
            .map_err(io("sync log directory"))
    }

    /// The last synced entry.
    pub(crate) fn last(&self) -> Option<LogPos> {
        let loc = self.locs.back()?;
        Some(LogPos {
            term: loc.term,
            index: self.first + self.locs.len() as u64 - 1,
        })
    }

    /// The synced entry at `index`, or `None` if there is none.
    pub(crate) fn read(&self, index: u64) -> Result<Option<LogEntry>, LogError> {
        let Some(loc) = index
            .checked_sub(self.first)
            .and_then(|at| self.locs.get(usize::try_from(at).ok()?))
        else {
            return Ok(None);
        };
        let mut bytes = vec![0; FORMAT.frame_bytes(loc.body_len)];
        self.segments[&loc.segment]
            .read_at(loc.offset, &mut bytes)
            .map_err(io("read entry"))?;
        let name = FORMAT.file_name(loc.segment);
        match FORMAT
            .decode_frame(&bytes)
            .and_then(|(body, _)| format::decode_body(body))
        {
            Some(Record::Entry(entry)) if entry.index == index => Ok(Some(entry)),
            _ => Err(corrupt(
                &name,
                loc.offset as usize,
                "entry does not read back",
            )),
        }
    }

    /// The index the next entry must have, if the disk holds a segment.
    fn next_index(&self) -> u64 {
        match self.group.len() {
            0 => self.first + self.locs.len() as u64,
            n => self.group_first + n as u64,
        }
    }

    /// At a group boundary, start a new segment if the active one is full.
    fn roll_if_full(&mut self) -> Result<(), LogError> {
        if !self.buf.is_empty() {
            return Ok(());
        }
        let Some((name, file)) = self.segments.last_key_value() else {
            return Ok(());
        };
        let holds_entries = self.locs.back().is_some_and(|loc| loc.segment == *name);
        if holds_entries && file.len() >= self.options.segment_bytes {
            self.create_segment(self.next_index())?;
        }
        Ok(())
    }

    fn create_segment(&mut self, first: u64) -> Result<(), LogError> {
        let file = self
            .fs
            .create_new(&FORMAT.path(&self.dir, first))
            .map_err(io("create segment"))?;
        self.segments.insert(first, file);
        self.buf.extend(FORMAT.encode_header(first));
        self.new_segment = true;
        Ok(())
    }

    /// Add an entry to the group. The caller keeps entries contiguous.
    pub(crate) fn push_entry(&mut self, entry: &LogEntry) -> Result<(), LogError> {
        self.roll_if_full()?;
        if self.segments.is_empty() {
            self.create_segment(entry.index)?;
        }
        let (name, file) = self.segments.last_key_value().expect("an active segment");
        let offset = file.len() + self.buf.len() as u64;
        let segment = *name;
        if self.group.is_empty() {
            self.group_first = entry.index;
        }
        let body_len = format::encode_entry(&mut self.buf, entry);
        self.group.push(Loc {
            segment,
            offset,
            body_len,
            term: entry.term,
        });
        Ok(())
    }

    /// Add a commit point to the group. With no segment yet, there is
    /// nowhere to write it, and it is dropped: the committed log id is a
    /// lower bound, so losing one is allowed.
    pub(crate) fn push_commit(&mut self, committed: LogPos) -> Result<(), LogError> {
        self.roll_if_full()?;
        if !self.segments.is_empty() {
            format::encode_commit(&mut self.buf, committed);
        }
        Ok(())
    }

    /// Write the group and make it durable with one sync.
    pub(crate) fn sync_group(&mut self) -> Result<(), LogError> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let file = self
            .segments
            .values_mut()
            .next_back()
            .expect("an active segment");
        file.append(&self.buf).map_err(io("write group"))?;
        file.sync_data().map_err(io("sync group"))?;
        if self.new_segment {
            self.sync_dir()?;
        }
        if self.locs.is_empty() {
            self.first = self.group_first;
        }
        self.locs.extend(self.group.drain(..));
        self.buf.clear();
        self.new_segment = false;
        Ok(())
    }

    /// Remove every entry after `after` (all of them for `None`), from the
    /// end backward: delete the segments that start above it, each removal
    /// synced, then cut the segment that holds the first removed entry.
    /// A crash in between leaves a longer log, never a hole.
    pub(crate) fn truncate_after(&mut self, after: Option<u64>) -> Result<(), LogError> {
        debug_assert!(self.buf.is_empty(), "sync the group first");
        let from = after.map_or(0, |after| after + 1);
        let doomed: Vec<u64> = self
            .segments
            .range(from..)
            .map(|(name, _)| *name)
            .rev()
            .collect();
        for name in doomed {
            self.segments.remove(&name);
            self.fs
                .remove(&FORMAT.path(&self.dir, name))
                .map_err(io("remove segment"))?;
            self.sync_dir()?;
        }
        let keep = usize::try_from(from.saturating_sub(self.first)).unwrap_or(usize::MAX);
        if let Some(loc) = self.locs.get(keep).copied()
            && let Some(file) = self.segments.get_mut(&loc.segment)
        {
            file.set_len(loc.offset).map_err(io("truncate segment"))?;
            file.sync_data().map_err(io("sync truncated segment"))?;
        }
        self.locs.truncate(keep);
        Ok(())
    }

    /// Record the purge point in `state`, then delete every segment whose
    /// entries are all at or below it. A crash between the two leaves
    /// extra segments, which `open` deletes.
    pub(crate) fn purge(&mut self, upto: LogPos) -> Result<(), LogError> {
        debug_assert!(self.buf.is_empty(), "sync the group first");
        self.write_state(State {
            purged: Some(upto),
            ..self.state
        })?;
        self.remove_purged_segments()?;
        Ok(())
    }

    /// Delete the segments at or below the purge point and drop their
    /// entries from the index. Returns whether any was deleted (and the
    /// directory synced).
    fn remove_purged_segments(&mut self) -> Result<bool, LogError> {
        let Some(purged) = self.state.purged else {
            return Ok(false);
        };
        let keep_from = purged.index + 1;
        let names: Vec<u64> = self.segments.keys().copied().collect();
        let last_entry = self.last().map(|last| last.index);
        let mut removed = false;
        for (at, name) in names.iter().enumerate() {
            let below = match names.get(at + 1) {
                Some(next) => *next <= keep_from,
                None => last_entry.is_none_or(|last| last < keep_from),
            };
            if !below {
                break;
            }
            self.segments.remove(name);
            self.fs
                .remove(&FORMAT.path(&self.dir, *name))
                .map_err(io("remove segment"))?;
            removed = true;
        }
        if removed {
            self.sync_dir()?;
        }
        while self.first < keep_from && self.locs.pop_front().is_some() {
            self.first += 1;
        }
        if self.locs.is_empty() {
            self.first = self.first.max(keep_from);
        }
        Ok(removed)
    }

    pub(crate) fn save_vote(&mut self, vote: StoredVote) -> Result<(), LogError> {
        debug_assert!(self.buf.is_empty(), "sync the group first");
        self.write_state(State {
            vote: Some(vote),
            ..self.state
        })
    }

    /// Replace `state` atomically: write `state.tmp`, sync it, rename it
    /// over `state`, and sync the directory.
    fn write_state(&mut self, state: State) -> Result<(), LogError> {
        let tmp = self.dir.join(STATE_TMP);
        match self.fs.remove(&tmp) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            other => other.map_err(io("remove state.tmp"))?,
        }
        let mut file = self.fs.create_new(&tmp).map_err(io("create state.tmp"))?;
        file.append(&format::encode_state(&state))
            .map_err(io("write state.tmp"))?;
        file.sync_data().map_err(io("sync state.tmp"))?;
        self.fs
            .rename(&tmp, &self.dir.join(STATE))
            .map_err(io("rename state"))?;
        self.sync_dir()?;
        self.state = state;
        Ok(())
    }
}
