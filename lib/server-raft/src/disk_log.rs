//! `DiskLog`: openraft's `RaftLogStorage` over the on-disk Raft log (plan
//! 0029, "The adapter and the drivers").
//!
//! `server_raft_log::RaftLog` knows nothing about openraft; this adapter
//! converts entries and votes, and drives durability:
//!
//! - **`append`** queues the entries, records the `IOFlushed` callback
//!   under the write's number, and wakes the driver, which fires the
//!   callback once a flush reports the write durable.
//! - **`save_vote`** queues the vote and waits on a callback of its own,
//!   so no async worker blocks on a sync.
//! - **The driver** is a thread named `raft-log-flusher` in production
//!   ([`Flusher::Thread`]): it waits for work, flushes, and fires the
//!   callbacks; groups form while a sync runs. In the simulator it is a
//!   task after a seeded delay (`Flusher::Task`), as `MemLog` does.
//! - **Reads** come from the entry cache. A miss reads the segments on the
//!   blocking pool with the thread driver, and inline with the task driver,
//!   whose file system never blocks.
//! - **A failed write or sync is sticky.** The log refuses every later
//!   write, waiting callbacks report the error, and openraft stops the
//!   node.

use std::fmt::Debug;
use std::io;
use std::marker::PhantomData;
use std::ops::Bound;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::thread::JoinHandle;
#[cfg(any(test, feature = "sim"))]
use std::time::Duration;

use openraft::AsyncRuntime;
use openraft::LogState;
use openraft::RaftLogReader;
use openraft::entry::RaftEntry;
use openraft::storage::IOFlushed;
use openraft::storage::RaftLogStorage;
use openraft::type_config::TypeConfigExt;
use openraft::vote::RaftLeaderId;
use server_raft_log::EntryKind;
use server_raft_log::Fs;
use server_raft_log::LogEntry;
use server_raft_log::LogError;
use server_raft_log::LogOptions;
use server_raft_log::LogPos;
use server_raft_log::RaftLog;
use server_raft_log::RealFs;
use server_raft_log::StoredVote;
use server_raft_log::WriteId;

use crate::command::Command;
use crate::encoding::Reader;
use crate::encoding::decode_membership_config;
use crate::encoding::encode_membership_config;
use crate::types::OrEntry;
use crate::types::OrLogId;
use crate::types::OrPayload;
use crate::types::OrVote;
use crate::types::Types;

/// How a [`DiskLog`] makes queued writes durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flusher {
    /// A thread named `raft-log-flusher`: production.
    Thread,
    /// A task on the runtime after a delay drawn uniformly from zero to
    /// `max_delay`; zero flushes inline. For the deterministic simulator.
    #[cfg(any(test, feature = "sim"))]
    Task { max_delay: Duration, seed: u64 },
}

fn to_io(error: LogError) -> io::Error {
    io::Error::other(error)
}

fn pos(log_id: &OrLogId) -> LogPos {
    LogPos {
        term: log_id.leader_id.term,
        index: log_id.index,
    }
}

fn log_id(pos: LogPos) -> OrLogId {
    OrLogId::new_term_index(pos.term, pos.index)
}

pub(crate) fn to_log_entry(entry: &OrEntry) -> Result<LogEntry, io::Error> {
    let (kind, payload) = match &entry.payload {
        OrPayload::Blank => (EntryKind::Blank, Vec::new()),
        OrPayload::Normal(command) => (
            EntryKind::Normal,
            command.to_bytes().map_err(io::Error::other)?,
        ),
        OrPayload::Membership(membership) => {
            let mut payload = Vec::new();
            encode_membership_config(&mut payload, membership);
            (EntryKind::Membership, payload)
        }
    };
    Ok(LogEntry {
        index: entry.log_id.index,
        term: entry.log_id.leader_id.term,
        kind,
        payload,
    })
}

pub(crate) fn to_or_entry(entry: LogEntry) -> Result<OrEntry, io::Error> {
    let payload = match entry.kind {
        EntryKind::Blank => OrPayload::Blank,
        EntryKind::Normal => {
            OrPayload::Normal(Command::decode(&entry.payload).map_err(io::Error::other)?)
        }
        EntryKind::Membership => {
            let mut reader = Reader(&entry.payload);
            OrPayload::Membership(decode_membership_config(&mut reader)?)
        }
    };
    Ok(OrEntry::new(log_id(entry.pos()), payload))
}

fn to_stored_vote(vote: &OrVote) -> StoredVote {
    StoredVote {
        term: vote.leader_id.term(),
        node: Some(*vote.leader_id.node_id()),
        committed: vote.committed,
    }
}

fn to_or_vote(vote: StoredVote) -> OrVote {
    let node = vote.node.unwrap_or_default();
    match vote.committed {
        true => OrVote::new_committed(vote.term, node),
        false => OrVote::new(vote.term, node),
    }
}

/// What the driver shares with the handles.
struct Shared<RT: AsyncRuntime, F: Fs> {
    log: RaftLog<F>,
    /// Callbacks waiting for their write to become durable, in order.
    waiting: Mutex<Vec<(WriteId, IOFlushed<Types<RT>>)>>,
    flusher: Flusher,
    /// Set when there is work for the flusher thread, or it must stop.
    wake: Mutex<Wake>,
    wakeup: Condvar,
    #[cfg(any(test, feature = "sim"))]
    rng: Mutex<crate::rng::Rng>,
    #[cfg(any(test, feature = "sim"))]
    observer: Mutex<crate::sim::Observed>,
}

#[derive(Default)]
struct Wake {
    work: bool,
    stop: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<RT: AsyncRuntime, F: Fs> Shared<RT, F> {
    /// Flush, then fire every callback whose write is durable; after a
    /// failure, fire every waiting callback with the error.
    fn flush_and_complete(&self) {
        let result = self.log.flush();
        let durable = self.log.durable();
        let (done, failed) = {
            let mut waiting = lock(&self.waiting);
            let (done, rest): (Vec<_>, Vec<_>) =
                waiting.drain(..).partition(|(id, _)| *id <= durable);
            match result {
                Ok(_) => {
                    *waiting = rest;
                    (done, Vec::new())
                }
                Err(_) => (done, rest),
            }
        };
        for (_, callback) in done {
            callback.io_completed(Ok(()));
        }
        if let Err(error) = result {
            let message = error.to_string();
            for (_, callback) in failed {
                callback.io_completed(Err(io::Error::other(message.clone())));
            }
        }
    }

    /// Ask the driver to make `id` durable, firing `callback` then.
    fn request(self: &Arc<Self>, id: WriteId, callback: Option<IOFlushed<Types<RT>>>) {
        if let Some(callback) = callback {
            lock(&self.waiting).push((id, callback));
        }
        match self.flusher {
            Flusher::Thread => {
                lock(&self.wake).work = true;
                self.wakeup.notify_one();
            }
            #[cfg(any(test, feature = "sim"))]
            Flusher::Task { max_delay, .. } => {
                let max = max_delay.as_micros() as u64;
                if max == 0 {
                    self.flush_and_complete();
                    return;
                }
                let delay = Duration::from_micros(lock(&self.rng).below(max + 1));
                let shared = Arc::clone(self);
                // Detached: a later flush covers this one if it is lost.
                let _flush = RT::spawn(async move {
                    RT::sleep(delay).await;
                    shared.flush_and_complete();
                });
            }
        }
    }

    /// The flusher thread's loop.
    fn run(&self) {
        loop {
            {
                let mut wake = lock(&self.wake);
                while !wake.work && !wake.stop {
                    wake = self
                        .wakeup
                        .wait(wake)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                if wake.stop && !wake.work {
                    break;
                }
                wake.work = false;
            }
            self.flush_and_complete();
        }
        self.flush_and_complete();
    }
}

/// Stops the flusher thread when the last handle is dropped.
struct Owner<RT: AsyncRuntime, F: Fs> {
    shared: Arc<Shared<RT, F>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl<RT: AsyncRuntime, F: Fs> Drop for Owner<RT, F> {
    fn drop(&mut self) {
        lock(&self.shared.wake).stop = true;
        self.shared.wakeup.notify_one();
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

/// openraft's log storage over the on-disk Raft log. Clones share one
/// log.
pub struct DiskLog<RT: AsyncRuntime, F: Fs = RealFs> {
    shared: Arc<Shared<RT, F>>,
    _owner: Arc<Owner<RT, F>>,
    _rt: PhantomData<fn() -> RT>,
}

impl<RT: AsyncRuntime, F: Fs> Clone for DiskLog<RT, F> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            _owner: Arc::clone(&self._owner),
            _rt: PhantomData,
        }
    }
}

impl<RT: AsyncRuntime, F: Fs> DiskLog<RT, F> {
    /// Open the log in `dir` over `fs`, recovering it, and start its
    /// driver.
    pub fn open(
        fs: F,
        dir: &Path,
        options: LogOptions,
        flusher: Flusher,
    ) -> Result<Self, io::Error> {
        let (log, _recovered) = RaftLog::open(fs, dir, options).map_err(to_io)?;
        let shared = Arc::new(Shared {
            log,
            waiting: Mutex::new(Vec::new()),
            flusher,
            wake: Mutex::new(Wake::default()),
            wakeup: Condvar::new(),
            #[cfg(any(test, feature = "sim"))]
            rng: Mutex::new(crate::rng::Rng::new(match flusher {
                Flusher::Task { seed, .. } => seed,
                Flusher::Thread => 0,
            })),
            #[cfg(any(test, feature = "sim"))]
            observer: Mutex::new(None),
        });
        let thread = match flusher {
            Flusher::Thread => {
                let driver = Arc::clone(&shared);
                Some(
                    std::thread::Builder::new()
                        .name("raft-log-flusher".to_owned())
                        .spawn(move || driver.run())?,
                )
            }
            #[cfg(any(test, feature = "sim"))]
            Flusher::Task { .. } => None,
        };
        let owner = Arc::new(Owner {
            shared: Arc::clone(&shared),
            thread: Mutex::new(thread),
        });
        Ok(Self {
            shared,
            _owner: owner,
            _rt: PhantomData,
        })
    }

    /// Make every write queued so far durable, now: the hook of a
    /// state-machine checkpoint (plan 0025, decision 2).
    pub fn sync(&self) -> Result<(), io::Error> {
        self.shared.flush_and_complete();
        match self.shared.log.durable() >= self.shared.log.submitted() {
            true => Ok(()),
            false => Err(io::Error::other("the Raft log failed")),
        }
    }

    fn after_write(&self, id: WriteId) {
        if self.shared.flusher == Flusher::Thread {
            self.shared.request(id, None);
        }
    }
}

/// What the simulator's checks need (plan 0028's models report the same).
#[cfg(any(test, feature = "sim"))]
impl<RT: AsyncRuntime, F: Fs> DiskLog<RT, F> {
    /// Report this log's durable votes to `observer` as `node`'s.
    // Only the simulator, which is test-only, observes the logs.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn observe(
        &self,
        node: crate::types::NodeId,
        observer: Arc<dyn crate::sim::Observer>,
    ) {
        *lock(&self.shared.observer) = Some((node, observer));
    }

    /// What readers see now.
    pub fn live(&self) -> crate::sim::LogImage {
        let log = &self.shared.log;
        let state = log.state();
        let entries = log
            .entries(..)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| to_or_entry(entry).ok())
            .map(|entry| (entry.log_id.index, entry))
            .collect();
        crate::sim::LogImage {
            vote: state.vote.map(to_or_vote),
            entries,
            purged: state.purged.map(log_id),
            committed: state.committed.map(log_id),
        }
    }

    /// Whether `id` is durable: synced, or below the durable purge point.
    pub fn is_durable(&self, id: OrLogId) -> bool {
        self.shared.log.is_durable(pos(&id))
    }

    /// The durable vote.
    pub fn durable_vote(&self) -> Option<OrVote> {
        self.shared.log.durable_vote().map(to_or_vote)
    }

    /// Whether a write or sync failed, so the log refuses writes.
    pub fn is_failed(&self) -> bool {
        self.shared.log.is_failed()
    }
}

impl<RT: AsyncRuntime, F: Fs> RaftLogReader<Types<RT>> for DiskLog<RT, F> {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<OrEntry>, io::Error> {
        let bounds = (range.start_bound().cloned(), range.end_bound().cloned());
        let start = match bounds.0 {
            Bound::Included(start) => start,
            Bound::Excluded(start) => start.saturating_add(1),
            Bound::Unbounded => 0,
        };
        let cached = start >= self.shared.log.cached_from();
        let entries = match self.shared.flusher {
            Flusher::Thread if !cached => {
                let shared = Arc::clone(&self.shared);
                RT::spawn_blocking(move || shared.log.entries(bounds)).await?
            }
            _ => self.shared.log.entries(bounds),
        };
        entries
            .map_err(to_io)?
            .into_iter()
            .map(to_or_entry)
            .collect()
    }

    async fn read_vote(&mut self) -> Result<Option<OrVote>, io::Error> {
        Ok(self.shared.log.state().vote.map(to_or_vote))
    }
}

impl<RT: AsyncRuntime, F: Fs> RaftLogStorage<Types<RT>> for DiskLog<RT, F> {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<Types<RT>>, io::Error> {
        let state = self.shared.log.state();
        Ok(LogState {
            last_purged_log_id: state.purged.map(log_id),
            last_log_id: state.last.map(log_id),
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &OrVote) -> Result<(), io::Error> {
        let id = self
            .shared
            .log
            .save_vote(to_stored_vote(vote))
            .map_err(to_io)?;
        let (tx, rx) = Types::<RT>::oneshot();
        self.shared.request(id, Some(IOFlushed::signal(tx)));
        rx.await
            .map_err(|_| io::Error::other("the Raft log flusher stopped"))??;
        #[cfg(any(test, feature = "sim"))]
        if let Some((node, observer)) = lock(&self.shared.observer).clone() {
            observer.vote_saved(node, *vote, &self.live());
        }
        Ok(())
    }

    async fn save_committed(&mut self, committed: Option<OrLogId>) -> Result<(), io::Error> {
        let id = self
            .shared
            .log
            .save_committed(committed.as_ref().map(pos))
            .map_err(to_io)?;
        self.after_write(id);
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<OrLogId>, io::Error> {
        Ok(self.shared.log.state().committed.map(log_id))
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: IOFlushed<Types<RT>>,
    ) -> Result<(), io::Error>
    where
        I: IntoIterator<Item = OrEntry> + Send,
        I::IntoIter: Send,
    {
        let entries = entries
            .into_iter()
            .map(|entry| to_log_entry(&entry))
            .collect::<Result<Vec<_>, _>>()?;
        let id = self.shared.log.append(entries).map_err(to_io)?;
        self.shared.request(id, Some(callback));
        Ok(())
    }

    async fn truncate_after(&mut self, last_log_id: Option<OrLogId>) -> Result<(), io::Error> {
        let id = self
            .shared
            .log
            .truncate_after(last_log_id.as_ref().map(pos))
            .map_err(to_io)?;
        self.after_write(id);
        Ok(())
    }

    async fn purge(&mut self, log_id: OrLogId) -> Result<(), io::Error> {
        let id = self.shared.log.purge(pos(&log_id)).map_err(to_io)?;
        self.after_write(id);
        Ok(())
    }
}
