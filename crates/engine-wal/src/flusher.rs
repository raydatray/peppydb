//! The writer: cheap appends into a pending buffer, and one flusher thread
//! that writes and syncs groups of records (group commit).
//!
//! Lock rules: appenders and the flusher share `pending`; durability
//! waiters use `durable_lock` only. Nothing holds `pending` while waiting on
//! `durable`, or `durable_lock` while taking `pending`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use super::record::{
    HEADER_BYTES, MAX_RECORD_BYTES, PREFIX_BYTES, batch_bytes, encode_batch, encode_header,
    encode_op, op_bytes,
};
use super::segment::{self, io};
use super::{Op, Wal, WalError};

/// Tuning for [`Wal::open`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalOptions {
    /// Wait this long after the first pending append before writing. Zero
    /// (the default) writes as soon as the previous sync ends: groups still
    /// form, from the appends that arrive during a sync.
    pub flush_interval: Duration,
    /// Seal the active segment once it holds at least this many bytes. A
    /// segment may exceed it by one group. Default 64 MiB.
    pub segment_bytes: u64,
}

impl Default for WalOptions {
    fn default() -> Self {
        Self {
            flush_interval: Duration::ZERO,
            segment_bytes: 64 << 20,
        }
    }
}

const OPEN: u8 = 0;
const CLOSING: u8 = 1;
const ABANDONED: u8 = 2;

/// Spare buffers above this capacity are released after a group, so a
/// burst does not pin memory.
const SPARE_LIMIT: usize = 4 << 20;

pub(crate) struct Shared {
    dir: PathBuf,
    options: WalOptions,
    pending: Mutex<Pending>,
    /// The flusher waits here, paired with `pending`.
    wake: Condvar,
    /// The highest synced sequence.
    durable_seq: AtomicU64,
    durable_lock: Mutex<()>,
    durable: Condvar,
    /// A `sync_to` target: the flusher skips the interval wait for it.
    force_to: AtomicU64,
    /// The highest assigned sequence.
    last_seq: AtomicU64,
    /// Bytes appended since open, for the engine's checkpoint policy.
    appended_bytes: AtomicU64,
    state: AtomicU8,
    /// The flusher has exited.
    stopped: AtomicBool,
    /// Sticky: every later call reports it.
    failed: OnceLock<(&'static str, std::io::ErrorKind)>,
    /// The original error, returned once by `close`.
    first_error: Mutex<Option<WalError>>,
    /// Syncs of a group, for tests.
    syncs: AtomicU64,
    /// Make the next group's sync fail, for tests.
    #[cfg(test)]
    pub(crate) fail_sync: AtomicBool,
}

struct Pending {
    buf: Vec<u8>,
    next_seq: u64,
    /// Names a new segment when the group rotates.
    first_in_buf: Option<u64>,
    first_pending_at: Option<Instant>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Wal {
    /// Open the log in `dir` for appends, continuing at `next_seq`: one more
    /// than both the highest replayed sequence and the tree's applied
    /// sequence. Run [`Wal::replay`] first. Starts the flusher thread.
    pub fn open(dir: &Path, options: WalOptions, next_seq: u64) -> Result<Wal, WalError> {
        // Sequences start at 1.
        let next_seq = next_seq.max(1);
        std::fs::create_dir_all(dir).map_err(io("create WAL directory"))?;
        // A header-only last segment holds nothing (a crash right after a
        // segment was created): replace it with one named `next_seq`.
        if let Some((_, path)) = segment::list(dir)?.last()
            && std::fs::metadata(path).map_err(io("stat segment"))?.len() <= HEADER_BYTES as u64
        {
            std::fs::remove_file(path).map_err(io("remove empty segment"))?;
            segment::sync_dir(dir)?;
        }
        let file = create(dir, next_seq)?;
        let shared = Arc::new(Shared {
            dir: dir.to_path_buf(),
            options,
            pending: Mutex::new(Pending {
                buf: Vec::new(),
                next_seq,
                first_in_buf: None,
                first_pending_at: None,
            }),
            wake: Condvar::new(),
            durable_seq: AtomicU64::new(next_seq - 1),
            durable_lock: Mutex::new(()),
            durable: Condvar::new(),
            force_to: AtomicU64::new(0),
            last_seq: AtomicU64::new(next_seq - 1),
            appended_bytes: AtomicU64::new(0),
            state: AtomicU8::new(OPEN),
            stopped: AtomicBool::new(false),
            failed: OnceLock::new(),
            first_error: Mutex::new(None),
            syncs: AtomicU64::new(0),
            #[cfg(test)]
            fail_sync: AtomicBool::new(false),
        });
        let flusher = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("wal-flusher".into())
                .spawn(move || flush_loop(&shared, file))
                .map_err(io("spawn flusher"))?
        };
        Ok(Wal {
            shared,
            flusher: Mutex::new(Some(flusher)),
        })
    }

    /// Assign the next sequence and copy the encoded record into the pending
    /// buffer. No I/O, so it is safe to call under a tree leaf lock. The
    /// record is durable once [`Wal::wait_durable`] returns for it.
    pub fn append(&self, op: Op<'_>) -> Result<u64, WalError> {
        check_size(op_bytes(op))?;
        self.append_with(|buf, seq| encode_op(buf, seq, op))
    }

    /// Append a batch as one record with one sequence. A batch larger than
    /// [`MAX_RECORD_BYTES`] is rejected before it takes a sequence.
    pub fn append_batch(&self, ops: &[Op<'_>]) -> Result<u64, WalError> {
        check_size(batch_bytes(ops))?;
        self.append_with(|buf, seq| encode_batch(buf, seq, ops))
    }

    fn append_with(&self, encode: impl FnOnce(&mut Vec<u8>, u64)) -> Result<u64, WalError> {
        self.check_open()?;
        let mut pending = lock(&self.shared.pending);
        // Re-check under the lock: a close drains only what it sees.
        self.check_open()?;
        let seq = pending.next_seq;
        let before = pending.buf.len();
        encode(&mut pending.buf, seq);
        let added = (pending.buf.len() - before) as u64;
        pending.next_seq += 1;
        pending.first_in_buf.get_or_insert(seq);
        pending.first_pending_at.get_or_insert_with(Instant::now);
        self.shared.last_seq.store(seq, Ordering::Release);
        drop(pending);
        self.shared
            .appended_bytes
            .fetch_add(added, Ordering::Relaxed);
        self.shared.wake.notify_one();
        Ok(seq)
    }

    /// Wait until the record `seq` is durable.
    pub fn wait_durable(&self, seq: u64) -> Result<(), WalError> {
        if self.shared.durable_seq.load(Ordering::Acquire) >= seq {
            return Ok(());
        }
        let mut guard = lock(&self.shared.durable_lock);
        loop {
            if self.shared.durable_seq.load(Ordering::Acquire) >= seq {
                return Ok(());
            }
            if let Some(&(op, kind)) = self.shared.failed.get() {
                return Err(WalError::Failed { op, kind });
            }
            if self.shared.stopped.load(Ordering::Acquire) {
                return Err(WalError::Closed);
            }
            guard = self
                .shared
                .durable
                .wait(guard)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Make every record up to `seq` durable now, skipping the flush
    /// interval. A checkpoint's pre-commit hook calls it.
    pub fn sync_to(&self, seq: u64) -> Result<(), WalError> {
        let seq = seq.min(self.last_seq());
        self.shared.force_to.fetch_max(seq, Ordering::AcqRel);
        // Under the pending lock, so the flusher cannot miss the wakeup
        // between its check and its wait.
        drop(lock(&self.shared.pending));
        self.shared.wake.notify_one();
        self.wait_durable(seq)
    }

    /// The highest durable sequence.
    pub fn durable_seq(&self) -> u64 {
        self.shared.durable_seq.load(Ordering::Acquire)
    }

    /// The highest assigned sequence.
    pub fn last_seq(&self) -> u64 {
        self.shared.last_seq.load(Ordering::Acquire)
    }

    /// Bytes appended since open. The engine's checkpoint policy compares
    /// it with its value at the last checkpoint.
    pub fn appended_bytes(&self) -> u64 {
        self.shared.appended_bytes.load(Ordering::Relaxed)
    }

    /// Delete every sealed segment whose successor starts at or below
    /// `applied + 1`: such a segment holds only records at or below
    /// `applied`. The last segment is never deleted. Deletions are not
    /// synced: one that does not survive a crash leaves a redundant segment
    /// that replay skips and the next truncation deletes. Concurrent calls
    /// are safe. Returns the number of segments deleted.
    pub fn truncate_below(&self, applied: u64) -> Result<usize, WalError> {
        let segments = segment::list(&self.shared.dir)?;
        let mut deleted = 0;
        for pair in segments.windows(2) {
            let ((_, path), (next_first, _)) = (&pair[0], &pair[1]);
            if *next_first > applied.saturating_add(1) {
                break;
            }
            match std::fs::remove_file(path) {
                Ok(()) => deleted += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io("delete segment")(error)),
            }
        }
        Ok(deleted)
    }

    /// Simulate a crash, for tests: stop the flusher after any group in
    /// flight, write the first `written` bytes of the pending records to the
    /// active segment without syncing, and drop the rest. Waiters for
    /// records that were never reported durable get [`WalError::Closed`].
    /// `written` can cut a record anywhere, which makes a torn tail.
    ///
    /// Production code calls [`Wal::close`], which makes every appended
    /// record durable. After `abandon`, appends return
    /// [`WalError::Closed`], and dropping the log writes nothing more.
    pub fn abandon(&self, written: usize) {
        self.shared.state.store(ABANDONED, Ordering::Release);
        drop(lock(&self.shared.pending));
        self.shared.wake.notify_all();
        let file = lock(&self.flusher)
            .take()
            .and_then(|flusher| flusher.join().ok())
            .flatten();
        let pending = lock(&self.shared.pending);
        if let Some(mut file) = file {
            let _ = file.write_all(&pending.buf[..written.min(pending.buf.len())]);
        }
    }

    /// Syncs so far, for tests.
    #[cfg(test)]
    pub(crate) fn syncs(&self) -> u64 {
        self.shared.syncs.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_sync(&self) {
        self.shared.fail_sync.store(true, Ordering::Relaxed);
    }

    /// Stop accepting appends, make every appended record durable, and stop
    /// the flusher. Returns the flusher's original error, once. Production
    /// code closes; `Drop` closes and ignores the result.
    pub fn close(&self) -> Result<(), WalError> {
        let _ =
            self.shared
                .state
                .compare_exchange(OPEN, CLOSING, Ordering::AcqRel, Ordering::Acquire);
        self.join_flusher();
        match lock(&self.shared.first_error).take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn join_flusher(&self) {
        drop(lock(&self.shared.pending));
        self.shared.wake.notify_all();
        if let Some(flusher) = lock(&self.flusher).take() {
            let _ = flusher.join();
        }
    }

    fn check_open(&self) -> Result<(), WalError> {
        if let Some(&(op, kind)) = self.shared.failed.get() {
            return Err(WalError::Failed { op, kind });
        }
        if self.shared.state.load(Ordering::Acquire) != OPEN {
            return Err(WalError::Closed);
        }
        Ok(())
    }
}

impl Drop for Wal {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn check_size(record_bytes: usize) -> Result<(), WalError> {
    if record_bytes - PREFIX_BYTES > MAX_RECORD_BYTES {
        return Err(WalError::RecordTooLarge {
            bytes: record_bytes,
        });
    }
    Ok(())
}

/// Create the segment for `first_seq`: write its header, sync it, and sync
/// the directory, so a segment that later holds durable records also has a
/// durable directory entry.
fn create(dir: &Path, first_seq: u64) -> Result<File, WalError> {
    let mut file = OpenOptions::new()
        .append(true)
        .create_new(true)
        .open(segment::path(dir, first_seq))
        .map_err(io("create segment"))?;
    file.write_all(&encode_header(first_seq))
        .map_err(io("write segment header"))?;
    file.sync_all().map_err(io("sync segment"))?;
    segment::sync_dir(dir)?;
    Ok(file)
}

/// The flusher thread. Returns the active segment, or `None` after a
/// failure.
fn flush_loop(shared: &Shared, mut file: File) -> Option<File> {
    let mut active_bytes = HEADER_BYTES as u64;
    let mut spare = Vec::new();
    loop {
        let mut pending = lock(&shared.pending);
        loop {
            let state = shared.state.load(Ordering::Acquire);
            if state == ABANDONED || (pending.buf.is_empty() && state == CLOSING) {
                drop(pending);
                stop(shared);
                return Some(file);
            }
            if pending.buf.is_empty() {
                pending = shared
                    .wake
                    .wait(pending)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            }
            // Wait out the interval, unless closing or a sync_to covers it.
            let interval = shared.options.flush_interval;
            let last = pending.next_seq - 1;
            let forced = shared.force_to.load(Ordering::Acquire) >= last;
            let deadline = pending.first_pending_at.expect("pending records") + interval;
            let now = Instant::now();
            if interval.is_zero() || state != OPEN || forced || now >= deadline {
                break;
            }
            pending = shared
                .wake
                .wait_timeout(pending, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        std::mem::swap(&mut pending.buf, &mut spare);
        let first = pending.first_in_buf.take().expect("pending records");
        let last = pending.next_seq - 1;
        pending.first_pending_at = None;
        drop(pending);

        let result = (|| {
            // An empty segment is never sealed: the next one would share
            // its name, the group's first sequence.
            let holds_records = active_bytes > HEADER_BYTES as u64;
            if holds_records && active_bytes >= shared.options.segment_bytes {
                // The old segment is synced, so it is sealed with no torn
                // tail; the group never spans two segments.
                file = create(&shared.dir, first)?;
                active_bytes = HEADER_BYTES as u64;
            }
            file.write_all(&spare).map_err(io("write WAL group"))?;
            #[cfg(test)]
            if shared.fail_sync.swap(false, Ordering::Relaxed) {
                return Err(io("sync WAL group")(std::io::Error::other("injected")));
            }
            file.sync_data().map_err(io("sync WAL group"))?;
            Ok::<_, WalError>(())
        })();
        if let Err(error) = result {
            if let WalError::Io { op, source } = &error {
                let _ = shared.failed.set((op, source.kind()));
            }
            *lock(&shared.first_error) = Some(error);
            stop(shared);
            return None;
        }
        shared.syncs.fetch_add(1, Ordering::Relaxed);
        active_bytes += spare.len() as u64;
        shared.durable_seq.store(last, Ordering::Release);
        drop(lock(&shared.durable_lock));
        shared.durable.notify_all();
        spare.clear();
        if spare.capacity() > SPARE_LIMIT {
            spare = Vec::new();
        }
    }
}

/// The flusher exits: wake every waiter so it sees why.
fn stop(shared: &Shared) {
    shared.stopped.store(true, Ordering::Release);
    drop(lock(&shared.durable_lock));
    shared.durable.notify_all();
}
