//! The embedded database (plan 0021): the BF-Tree and its write-ahead log
//! composed into [`Db`].
//!
//! # Consistency
//!
//! - **Single keys are linearizable.** A `get` returns the latest applied
//!   write to its key, and writes to one key apply in their log order.
//! - **Batches are atomic for durability, not for visibility.** After any
//!   crash a [`WriteBatch`] is wholly present or wholly absent. While it is
//!   being applied, a concurrent reader can see part of it: with `a` and
//!   `b` both 1, a reader of a batch that sets both to 2 can see `a = 2`
//!   and `b = 1`, a state no history contains.
//! - **Scans are consistent per leaf, not across leaves.** [`Db::range`]
//!   and [`Db::scan_with`] read one leaf at a time, and writes run between
//!   leaves. A scan can see a later write to one key and miss an earlier
//!   write to another: if a writer sets `x = 1` and then `y = 1` while a
//!   scan is between `x`'s leaf and `y`'s leaf, the scan returns `x = 0`
//!   and `y = 1`. It can also see part of a batch. Keys still come out in
//!   order, and each leaf's records are a consistent picture.
//! - **Reads can see writes that are not yet durable.** A write is applied
//!   before its log sync; if the process crashes first, a reader may have
//!   seen a value that recovery does not restore. Its writer never received
//!   `Ok`.
//!
//! This is weaker than RocksDB and LevelDB, whose batches become visible
//! all at once and whose iterators read from an implicit snapshot. Code
//! that keeps an invariant across keys (an index and its record, a move
//! between two keys) or that scans while writing must not rely on either.
//! [`Db::write_snapshot`] gives a consistent point-in-time copy.
//!
//! ```
//! let dir = std::env::temp_dir().join(format!("engine-doc-{}", std::process::id()));
//! let db = engine::Db::open(&dir, engine::Options::default()).unwrap();
//! assert_eq!(db.get(b"k").unwrap(), None);
//! db.close().unwrap();
//! std::fs::remove_dir_all(&dir).unwrap();
//! ```

use std::collections::BTreeMap;
use std::fs::File;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use bf_tree::{Entry, FileStore, Superblock, Tree};
use engine_wal::{Wal, WalOptions};

pub use bf_tree::{LogError, Scan, TreeConfig, TreeError};
pub use engine_wal::WalError;

#[cfg(test)]
mod tests;

/// How the database is opened. Tree limits are fixed when the database is
/// created; a reopen must pass the same ones.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Tree limits and runtime knobs.
    pub tree: TreeConfig,
    pub mode: Mode,
    /// The WAL flusher waits this long after the first pending write.
    /// Zero (the default) syncs as soon as the previous sync ends.
    pub flush_interval: Duration,
    /// Seal a WAL segment once it holds this many bytes. Default 64 MiB.
    pub segment_bytes: u64,
    /// Checkpoint once this many WAL bytes were appended since the last
    /// one. Default 256 MiB.
    pub checkpoint_wal_bytes: u64,
    /// Checkpoint at least this often while writes arrive. Default 60 s.
    pub checkpoint_interval: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            tree: TreeConfig::default(),
            mode: Mode::Standalone,
            flush_interval: Duration::ZERO,
            segment_bytes: 64 << 20,
            checkpoint_wal_bytes: 256 << 20,
            checkpoint_interval: Duration::from_secs(60),
        }
    }
}

impl Options {
    fn validate(&self) -> Result<(), Error> {
        if self.segment_bytes < 4096 {
            return Err(Error::Options("segment_bytes must be at least 4096"));
        }
        if self.checkpoint_wal_bytes == 0 {
            return Err(Error::Options("checkpoint_wal_bytes must be positive"));
        }
        if self.checkpoint_interval.is_zero() {
            return Err(Error::Options("checkpoint_interval must be positive"));
        }
        self.tree.validate()?;
        Ok(())
    }

    fn wal(&self) -> WalOptions {
        WalOptions {
            flush_interval: self.flush_interval,
            segment_bytes: self.segment_bytes,
        }
    }
}

/// Where the database's sequences and durability come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// The engine keeps a WAL; writes return once durable.
    #[default]
    Standalone,
}

/// A database failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Tree(TreeError),
    #[error(transparent)]
    Wal(#[from] WalError),
    #[error("I/O error during {op}: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("options disagree with the database: {0}")]
    Options(&'static str),
    #[error("the database is open in another process or handle")]
    Locked,
}

impl From<TreeError> for Error {
    /// A log failure inside a tree call is the WAL's error, not the tree's.
    fn from(error: TreeError) -> Self {
        match error {
            TreeError::Log(LogError::Closed) => Error::Wal(WalError::Closed),
            TreeError::Log(LogError::Io { op, kind, .. }) => {
                Error::Wal(WalError::Failed { op, kind })
            }
            TreeError::InvalidStore("store superblock differs from the configuration") => {
                Error::Options("tree limits differ from the store")
            }
            other => Error::Tree(other),
        }
    }
}

fn io(op: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Io { op, source }
}

/// The WAL as the tree's log. A local newtype: neither the trait nor `Wal`
/// belongs to this crate.
struct WalLog<'a>(&'a Wal);

impl bf_tree::Log for WalLog<'_> {
    /// Runs under the tree's leaf lock: the WAL's append does no I/O.
    fn append(&self, op: bf_tree::Op<'_>) -> Result<u64, LogError> {
        self.0
            .append(to_wal_op(&op))
            .map_err(|error| to_log_error(&error))
    }
}

/// The WAL's errors cross the tree as `LogError`, which is `Copy` like
/// `TreeError`. The mapping is lossless for the variants an append or a
/// sync produces: the WAL's sticky failure is only an operation name and an
/// `ErrorKind`.
fn to_log_error(error: &WalError) -> LogError {
    match *error {
        WalError::Closed => LogError::Closed,
        WalError::Failed { op, kind } => LogError::Io {
            op,
            kind,
            code: None,
        },
        _ => LogError::Other("wal"),
    }
}

/// The WAL's operation for a tree operation.
fn to_wal_op<'a>(op: &bf_tree::Op<'a>) -> engine_wal::Op<'a> {
    match *op {
        bf_tree::Op::Put { key, value } => engine_wal::Op::Put { key, value },
        bf_tree::Op::Delete { key } => engine_wal::Op::Delete { key },
    }
}

/// The tree's operation for a WAL operation.
fn to_tree_op<'a>(op: &engine_wal::Op<'a>) -> bf_tree::Op<'a> {
    match *op {
        engine_wal::Op::Put { key, value } => bf_tree::Op::Put { key, value },
        engine_wal::Op::Delete { key } => bf_tree::Op::Delete { key },
    }
}

/// Several writes that become durable together: after any crash, all or
/// none of them are present. Not isolated: a concurrent reader may see some
/// of them before others.
#[derive(Clone, Debug, Default)]
pub struct WriteBatch {
    ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace `key`.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> &mut Self {
        self.ops.push((key.to_vec(), Some(value.to_vec())));
        self
    }

    /// Remove `key`.
    pub fn delete(&mut self, key: &[u8]) -> &mut Self {
        self.ops.push((key.to_vec(), None));
        self
    }

    /// Operations added, duplicates included.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// The batch's effect: sorted by key, the last operation per key. Its
    /// meaning then does not depend on the order in which `apply` writes.
    fn normalize(self) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let last: BTreeMap<_, _> = self.ops.into_iter().collect();
        last.into_iter().collect()
    }
}

fn tree_ops(ops: &[(Vec<u8>, Option<Vec<u8>>)]) -> Vec<bf_tree::Op<'_>> {
    ops.iter()
        .map(|(key, value)| match value {
            Some(value) => bf_tree::Op::Put { key, value },
            None => bf_tree::Op::Delete { key },
        })
        .collect()
}

/// The embedded database.
pub struct Db {
    path: PathBuf,
    shared: Arc<Shared>,
    /// The checkpoint policy thread; `None` once stopped.
    policy_thread: Option<JoinHandle<()>>,
    /// Held for the database's lifetime; dropping it releases the lock.
    _lock: File,
}

/// What the database and its policy thread share.
struct Shared {
    tree: Tree,
    wal: Wal,
    /// Single writes hold it shared; a batch holds it exclusively from its
    /// WAL append until it is applied, so no single write to one of its
    /// keys can take a later sequence and apply first. Readers and
    /// checkpoints never take it.
    gate: RwLock<()>,
    policy: Policy,
    /// The first background checkpoint error, for `close`. Only the policy
    /// thread and `close` touch it; writers never do.
    background_error: Mutex<Option<Error>>,
}

/// A committed checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// Every write at or below this sequence is in the committed tree.
    pub applied_seq: u64,
    /// Increases by one per committed checkpoint.
    pub checkpoint_id: u64,
}

impl From<bf_tree::State> for Checkpoint {
    fn from(state: bf_tree::State) -> Self {
        Self {
            applied_seq: state.applied_seq,
            checkpoint_id: state.checkpoint_id,
        }
    }
}

/// When to checkpoint. Writers touch only its atomics, so a writer never
/// waits for a checkpoint; the policy thread sleeps with `park_timeout`.
struct Policy {
    /// WAL bytes appended since the last checkpoint began that trigger one.
    threshold: u64,
    interval: Duration,
    /// `wal.appended_bytes()` when the last checkpoint began.
    bytes_at_checkpoint: AtomicU64,
    /// The committed applied sequence of the last checkpoint.
    applied_at_checkpoint: AtomicU64,
    kicked: AtomicBool,
    stop: AtomicBool,
    thread: OnceLock<Thread>,
}

impl Policy {
    /// After a write: one relaxed load and a subtraction in the common case;
    /// at most one writer per threshold crossing unparks the thread.
    fn note_append(&self, wal: &Wal) {
        let grown = wal
            .appended_bytes()
            .saturating_sub(self.bytes_at_checkpoint.load(Ordering::Relaxed));
        if grown >= self.threshold
            && !self.kicked.load(Ordering::Relaxed)
            && !self.kicked.swap(true, Ordering::AcqRel)
            && let Some(thread) = self.thread.get()
        {
            thread.unpark();
        }
    }
}

impl Shared {
    /// Checkpoint the tree with WAL-before-data, then truncate the WAL.
    fn checkpoint(&self) -> Result<Checkpoint, Error> {
        // Writes during this checkpoint do not re-kick it for bytes it
        // already covers.
        self.policy
            .bytes_at_checkpoint
            .store(self.wal.appended_bytes(), Ordering::Relaxed);
        let state = self.tree.checkpoint_with(|point| {
            // Every effect in the pages being committed has a sequence at or
            // below `captured_seq`, and its record was appended before the
            // effect was applied: syncing through it makes every committed
            // effect durable in the WAL first.
            self.wal
                .sync_to(point.captured_seq)
                .map_err(|error| to_log_error(&error))
        })?;
        self.wal.truncate_below(state.applied_seq)?;
        self.policy
            .applied_at_checkpoint
            .store(state.applied_seq, Ordering::Relaxed);
        Ok(state.into())
    }

    /// The policy thread: checkpoint when enough WAL bytes arrived, or when
    /// the interval passed and something new was applied.
    fn run_policy(&self) {
        let mut deadline = Instant::now() + self.policy.interval;
        loop {
            std::thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
            if self.policy.stop.load(Ordering::Acquire) {
                return;
            }
            let grown = self.policy.kicked.swap(false, Ordering::AcqRel)
                || self
                    .wal
                    .appended_bytes()
                    .saturating_sub(self.policy.bytes_at_checkpoint.load(Ordering::Relaxed))
                    >= self.policy.threshold;
            let now = Instant::now();
            let due = now >= deadline
                && self.tree.applied_seq()
                    > self.policy.applied_at_checkpoint.load(Ordering::Relaxed);
            if (grown || due)
                && let Err(error) = self.checkpoint()
            {
                let mut stored = self
                    .background_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                stored.get_or_insert(error);
            }
            if now >= deadline {
                deadline = now + self.policy.interval;
            }
        }
    }
}

// Compile-time check: a database is shared between threads.
const _: fn() = || {
    fn shared<T: Send + Sync>() {}
    shared::<Db>();
};

impl Db {
    /// Open or create the database at `path`, and recover it: open the tree
    /// at its last checkpoint, replay the WAL above it, and continue the WAL.
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Db, Error> {
        options.validate()?;
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path).map_err(io("create database directory"))?;
        let lock = lock_file(&path)?;
        let tree = open_or_create_tree(&path, &options)?;
        let applied = tree.applied_seq();
        let wal_dir = path.join("wal");
        let replayed = Wal::replay(&wal_dir, applied + 1, |record| {
            let ops: Vec<_> = record.ops.iter().map(to_tree_op).collect();
            tree.apply(Entry {
                seq: record.seq,
                ops: &ops,
            })
            .map_err(Error::from)
        })?;
        let next = applied.max(replayed.last_seq.unwrap_or(0)) + 1;
        let wal = Wal::open(&wal_dir, options.wal(), next)?;
        wal.truncate_below(applied)?;
        let shared = Arc::new(Shared {
            policy: Policy {
                threshold: options.checkpoint_wal_bytes,
                interval: options.checkpoint_interval,
                bytes_at_checkpoint: AtomicU64::new(wal.appended_bytes()),
                applied_at_checkpoint: AtomicU64::new(applied),
                kicked: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                thread: OnceLock::new(),
            },
            tree,
            wal,
            gate: RwLock::new(()),
            background_error: Mutex::new(None),
        });
        let policy_thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("engine-checkpoint".into())
                .spawn(move || shared.run_policy())
                .map_err(io("spawn the checkpoint thread"))?
        };
        let _ = shared.policy.thread.set(policy_thread.thread().clone());
        Ok(Db {
            path,
            shared,
            policy_thread: Some(policy_thread),
            _lock: lock,
        })
    }

    /// Commit a checkpoint now: the tree at every write applied so far, with
    /// every committed effect durable in the WAL first. Then delete WAL
    /// segments the checkpoint covers. The policy thread calls the same
    /// function; concurrent calls are safe.
    pub fn checkpoint(&self) -> Result<Checkpoint, Error> {
        self.shared.checkpoint()
    }

    /// Write the last committed checkpoint to `out` as a tree store file,
    /// while writes continue. Returns that checkpoint.
    pub fn write_snapshot(&self, out: &mut dyn std::io::Write) -> Result<Checkpoint, Error> {
        Ok(self.shared.tree.write_snapshot(out)?.into())
    }

    /// Simulate a process crash, for tests: stop the policy thread, abandon
    /// the WAL with only the first `written` bytes of its pending records
    /// written and none synced, and drop the tree without a checkpoint.
    #[cfg(test)]
    pub(crate) fn crash(mut self, written: usize) {
        self.stop_policy();
        self.shared.wal.abandon(written);
    }

    fn stop_policy(&mut self) {
        if let Some(thread) = self.policy_thread.take() {
            self.shared.policy.stop.store(true, Ordering::Release);
            thread.thread().unpark();
            let _ = thread.join();
        }
    }

    /// Insert or replace `key`. Returns once the write is durable in the
    /// WAL. The write is applied first, so a concurrent reader can see it
    /// before it is durable; a crash before the sync loses it, and this call
    /// never returned `Ok`.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        let seq = {
            let _single = self
                .shared
                .gate
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            self.shared
                .tree
                .put_logged(&WalLog(&self.shared.wal), key, value)?
        };
        // After the leaf lock is released: a slow sync holds no lock.
        self.shared.policy.note_append(&self.shared.wal);
        self.shared.wal.wait_durable(seq)?;
        Ok(())
    }

    /// Remove `key` if present. Returns once the removal is durable.
    pub fn delete(&self, key: &[u8]) -> Result<(), Error> {
        let seq = {
            let _single = self
                .shared
                .gate
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            self.shared
                .tree
                .remove_logged(&WalLog(&self.shared.wal), key)?
        };
        self.shared.policy.note_append(&self.shared.wal);
        self.shared.wal.wait_durable(seq)?;
        Ok(())
    }

    /// Apply a batch atomically: one WAL record with one sequence, then
    /// every operation. Every operation is checked against the tree's limits
    /// first, because a logged batch that the tree rejects would fail every
    /// later replay. Returns once the batch is durable.
    pub fn write(&self, batch: WriteBatch) -> Result<(), Error> {
        let ops = batch.normalize();
        if ops.is_empty() {
            return Ok(());
        }
        let tree_ops = tree_ops(&ops);
        tree_ops
            .iter()
            .try_for_each(|op| self.shared.tree.check_op(op))?;
        let wal_ops: Vec<_> = tree_ops.iter().map(to_wal_op).collect();
        let seq = {
            let _exclusive = self
                .shared
                .gate
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let seq = self.shared.wal.append_batch(&wal_ops)?;
            self.shared.tree.apply(Entry {
                seq,
                ops: &tree_ops,
            })?;
            seq
        };
        self.shared.policy.note_append(&self.shared.wal);
        self.shared.wal.wait_durable(seq)?;
        Ok(())
    }

    /// The value stored for `key`, or `None`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.shared.tree.get(key)?)
    }

    /// Call `f` exactly once with the value stored for `key`, or `None`.
    /// `f` runs under a leaf lock and must not call back into the database.
    pub fn get_with<R>(&self, key: &[u8], f: impl FnOnce(Option<&[u8]>) -> R) -> Result<R, Error> {
        Ok(self.shared.tree.get_with(key, f)?)
    }

    /// The records in `[start, end)`, in key order. Consistent per leaf,
    /// not across leaves.
    pub fn range(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<Scan<'_>, Error> {
        Ok(self.shared.tree.range(start, end)?)
    }

    /// Visit the records in `[start, end)` in key order without copying.
    /// `visit` runs under a leaf lock and must not call back into the
    /// database.
    pub fn scan_with(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        visit: impl FnMut(&[u8], &[u8]) -> ControlFlow<()>,
    ) -> Result<(), Error> {
        Ok(self.shared.tree.scan_with(start, end, visit)?)
    }

    /// Every write with a sequence at or below this is applied.
    pub fn applied_seq(&self) -> u64 {
        self.shared.tree.applied_seq()
    }

    /// The directory that holds the database.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Close the database: stop the policy thread, checkpoint, close the
    /// WAL, and release the lock. Returns the first error among the
    /// checkpoint, the WAL, and any earlier background checkpoint. A reopen
    /// after `close` replays nothing.
    pub fn close(mut self) -> Result<(), Error> {
        self.stop_policy();
        let checkpointed = self.shared.checkpoint();
        let closed = self.shared.wal.close();
        let background = self
            .shared
            .background_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        checkpointed?;
        closed?;
        background.map_or(Ok(()), Err)
    }
}

/// Take the database's lock file; `Locked` if another handle holds it.
fn lock_file(path: &Path) -> Result<File, Error> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.join("LOCK"))
        .map_err(io("open lock file"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::Locked),
        Err(std::fs::TryLockError::Error(error)) => Err(io("lock")(error)),
    }
}

fn open_or_create_tree(path: &Path, options: &Options) -> Result<Tree, Error> {
    let file = path.join("tree.bf");
    if file.exists() {
        let store = FileStore::open(&file).map_err(TreeError::from)?;
        return Ok(Tree::open_with(options.tree, Box::new(store))?);
    }
    let store =
        FileStore::create(&file, Superblock::from(&options.tree)).map_err(TreeError::from)?;
    Ok(Tree::create(options.tree, Box::new(store))?)
}

impl Drop for Db {
    /// Without `close`: stop the policy thread, and let the WAL drain when
    /// the last reference drops. No checkpoint; a reopen replays the WAL.
    fn drop(&mut self) {
        self.stop_policy();
    }
}
