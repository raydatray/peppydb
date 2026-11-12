//! `MemLog`: an in-memory Raft log with a crash model (plan 0028, "Storage
//! models").
//!
//! It stands in for the on-disk Raft log of plan 0025, S5, and keeps the
//! same durability contract:
//!
//! - **Writes are serialized.** Every write (append, truncate, purge, saved
//!   commit point, vote) joins one ordered queue. After a crash the durable
//!   log equals the result of some prefix of that queue: a write is never
//!   durable while an earlier one is lost, as openraft requires of
//!   `RaftLogStorage`.
//! - **Appends are readable at once and durable later.** `append` applies
//!   the entries to the live image and returns; a flush, after a delay drawn
//!   from the seed, makes them durable and only then fires their
//!   `IOFlushed` callbacks. This is the group commit of the S5 flusher.
//! - **A vote is durable before `save_vote` returns,** with every write
//!   queued before it.
//! - **A crash loses every write that is not durable** and freezes the log:
//!   every later call fails, so the node stops. The durable image restarts
//!   the node.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::io;
use std::marker::PhantomData;
use std::ops::RangeBounds;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;

use openraft::AsyncRuntime;
use openraft::LogState;
use openraft::RaftLogReader;
use openraft::storage::IOFlushed;
use openraft::storage::RaftLogStorage;

use crate::rng::Rng;
use crate::types::OrEntry;
use crate::types::OrLogId;
use crate::types::OrVote;
use crate::types::Types;

/// The contents of a log: what is readable now, or what survives a crash.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogImage {
    pub vote: Option<OrVote>,
    pub entries: BTreeMap<u64, OrEntry>,
    pub purged: Option<OrLogId>,
    pub committed: Option<OrLogId>,
}

impl LogImage {
    /// The last entry's log id, or the purge point if no entry remains.
    pub fn last_log_id(&self) -> Option<OrLogId> {
        self.entries
            .values()
            .next_back()
            .map(|entry| entry.log_id)
            .or(self.purged)
    }

    fn apply(&mut self, write: &Write) {
        match write {
            Write::Append(entries) => {
                for entry in entries {
                    self.entries.insert(entry.log_id.index, entry.clone());
                }
            }
            Write::Truncate(after) => {
                let from = after.map_or(0, |log_id| log_id.index + 1);
                self.entries.split_off(&from);
            }
            Write::Purge(upto) => {
                self.purged = self.purged.max(Some(*upto));
                self.entries = self.entries.split_off(&(upto.index + 1));
            }
            Write::Committed(committed) => self.committed = *committed,
            Write::Vote(vote) => self.vote = Some(*vote),
        }
    }
}

/// One serialized write.
#[derive(Debug, Clone)]
enum Write {
    Append(Vec<OrEntry>),
    Truncate(Option<OrLogId>),
    Purge(OrLogId),
    Committed(Option<OrLogId>),
    Vote(OrVote),
}

/// How a `MemLog` behaves.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Flushes come after a delay drawn uniformly from zero to this. Zero
    /// flushes inside `append`, before it returns.
    pub max_flush_delay: Duration,
    /// The seed of the flush delays.
    pub seed: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            max_flush_delay: Duration::ZERO,
            seed: 0,
        }
    }
}

/// A write that is not durable yet: its sequence number, the write, and
/// the callback to fire once it is durable.
struct Pending<RT: AsyncRuntime> {
    seq: u64,
    write: Write,
    callback: Option<IOFlushed<Types<RT>>>,
}

struct Inner<RT: AsyncRuntime> {
    durable: LogImage,
    live: LogImage,
    /// Writes not yet durable, in order.
    pending: VecDeque<Pending<RT>>,
    next_seq: u64,
    crashed: bool,
    rng: Rng,
    config: LogConfig,
}

impl<RT: AsyncRuntime> Inner<RT> {
    fn check(&self) -> Result<(), io::Error> {
        match self.crashed {
            true => Err(io::Error::other("the log crashed")),
            false => Ok(()),
        }
    }

    /// Apply `write` to the live image and queue it for durability.
    fn submit(&mut self, write: Write, callback: Option<IOFlushed<Types<RT>>>) -> u64 {
        self.live.apply(&write);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.push_back(Pending {
            seq,
            write,
            callback,
        });
        seq
    }

    /// Make every queued write up to `seq` durable, in order, then fire
    /// their callbacks.
    fn flush_through(&mut self, seq: u64) {
        if self.crashed {
            return;
        }
        let mut callbacks = Vec::new();
        while self
            .pending
            .front()
            .is_some_and(|pending| pending.seq <= seq)
        {
            let Some(pending) = self.pending.pop_front() else {
                break;
            };
            self.durable.apply(&pending.write);
            callbacks.extend(pending.callback);
        }
        for callback in callbacks {
            callback.io_completed(Ok(()));
        }
    }

    fn flush_all(&mut self) {
        if let Some(last) = self.next_seq.checked_sub(1) {
            self.flush_through(last);
        }
    }
}

/// An in-memory Raft log with a crash model. Clones share one log.
pub struct MemLog<RT: AsyncRuntime> {
    inner: Arc<Mutex<Inner<RT>>>,
    _rt: PhantomData<fn() -> RT>,
}

impl<RT: AsyncRuntime> Clone for MemLog<RT> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _rt: PhantomData,
        }
    }
}

impl<RT: AsyncRuntime> MemLog<RT> {
    /// An empty log.
    pub fn new(config: LogConfig) -> Self {
        Self::from_image(LogImage::default(), config)
    }

    /// A log restarted from the durable image of a crashed one.
    pub fn from_image(image: LogImage, config: LogConfig) -> Self {
        let inner = Inner {
            durable: image.clone(),
            live: image,
            pending: VecDeque::new(),
            next_seq: 0,
            crashed: false,
            rng: Rng::new(config.seed),
            config,
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            _rt: PhantomData,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner<RT>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What survives a crash now.
    pub fn durable(&self) -> LogImage {
        self.lock().durable.clone()
    }

    /// What readers see now.
    pub fn live(&self) -> LogImage {
        self.lock().live.clone()
    }

    /// Crash: drop every write that is not durable, freeze the log, and
    /// return what survives. Callbacks of lost writes never fire.
    pub fn crash(&self) -> LogImage {
        let mut inner = self.lock();
        inner.crashed = true;
        inner.pending.clear();
        inner.durable.clone()
    }

    /// Make every write queued so far durable: the hook of a state-machine
    /// checkpoint (plan 0025, decision 2).
    pub fn sync(&self) {
        self.lock().flush_all();
    }

    /// Schedule a flush through `seq`, after a delay drawn from the seed.
    fn schedule_flush(&self, seq: u64) {
        let delay = {
            let mut inner = self.lock();
            let max = inner.config.max_flush_delay.as_micros() as u64;
            if max == 0 {
                inner.flush_through(seq);
                return;
            }
            Duration::from_micros(inner.rng.below(max + 1))
        };
        let log = self.clone();
        // Detached: a crash makes the flush a no-op.
        let _flush = RT::spawn(async move {
            RT::sleep(delay).await;
            log.lock().flush_through(seq);
        });
    }
}

impl<RT: AsyncRuntime> RaftLogReader<Types<RT>> for MemLog<RT> {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<OrEntry>, io::Error> {
        let inner = self.lock();
        inner.check()?;
        Ok(inner
            .live
            .entries
            .range(range)
            .map(|(_, entry)| entry.clone())
            .collect())
    }

    async fn read_vote(&mut self) -> Result<Option<OrVote>, io::Error> {
        let inner = self.lock();
        inner.check()?;
        Ok(inner.live.vote)
    }
}

impl<RT: AsyncRuntime> RaftLogStorage<Types<RT>> for MemLog<RT> {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<Types<RT>>, io::Error> {
        let inner = self.lock();
        inner.check()?;
        Ok(LogState {
            last_purged_log_id: inner.live.purged,
            last_log_id: inner.live.last_log_id(),
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &OrVote) -> Result<(), io::Error> {
        let mut inner = self.lock();
        inner.check()?;
        inner.submit(Write::Vote(*vote), None);
        inner.flush_all();
        Ok(())
    }

    async fn save_committed(&mut self, committed: Option<OrLogId>) -> Result<(), io::Error> {
        let mut inner = self.lock();
        inner.check()?;
        inner.submit(Write::Committed(committed), None);
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<OrLogId>, io::Error> {
        let inner = self.lock();
        inner.check()?;
        Ok(inner.live.committed)
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
        let seq = {
            let mut inner = self.lock();
            inner.check()?;
            let entries: Vec<_> = entries.into_iter().collect();
            inner.submit(Write::Append(entries), Some(callback))
        };
        self.schedule_flush(seq);
        Ok(())
    }

    async fn truncate_after(&mut self, last_log_id: Option<OrLogId>) -> Result<(), io::Error> {
        let mut inner = self.lock();
        inner.check()?;
        inner.submit(Write::Truncate(last_log_id), None);
        Ok(())
    }

    async fn purge(&mut self, log_id: OrLogId) -> Result<(), io::Error> {
        let mut inner = self.lock();
        inner.check()?;
        inner.submit(Write::Purge(log_id), None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use openraft::entry::RaftEntry;
    use openraft::type_config::TypeConfigExt;
    use openraft_rt_sim::SimRuntime;

    use super::*;
    use crate::types::OrPayload;
    use crate::types::log_id;

    type Log = MemLog<SimRuntime>;

    fn blank(term: u64, index: u64) -> OrEntry {
        OrEntry::new(log_id(term, index), OrPayload::Blank)
    }

    fn indexes(image: &LogImage) -> Vec<u64> {
        image.entries.keys().copied().collect()
    }

    fn delayed() -> LogConfig {
        LogConfig {
            max_flush_delay: Duration::from_millis(5),
            seed: 1,
        }
    }

    #[test]
    fn appends_are_readable_at_once_and_durable_after_the_flush() {
        SimRuntime::with_seed(1).block_on(async {
            let mut log = Log::new(delayed());
            let (tx, rx) = Types::<SimRuntime>::oneshot();
            log.append([blank(1, 1), blank(1, 2)], IOFlushed::signal(tx))
                .await
                .unwrap();
            assert_eq!(indexes(&log.live()), [1, 2]);
            assert_eq!(indexes(&log.durable()), [0u64; 0]);

            rx.await.unwrap().unwrap();
            assert_eq!(
                indexes(&log.durable()),
                [1, 2],
                "durable when the callback fires"
            );
        });
    }

    #[test]
    fn a_crash_loses_exactly_the_unflushed_suffix() {
        SimRuntime::with_seed(2).block_on(async {
            let mut log = Log::new(delayed());
            log.append([blank(1, 1)], IOFlushed::noop()).await.unwrap();
            log.sync();
            log.append([blank(1, 2), blank(1, 3)], IOFlushed::noop())
                .await
                .unwrap();
            log.truncate_after(Some(log_id(1, 2))).await.unwrap();

            let survived = log.crash();
            assert_eq!(indexes(&survived), [1]);
            assert!(
                log.get_log_state().await.is_err(),
                "a crashed log refuses calls"
            );

            let mut restarted = Log::from_image(survived, delayed());
            let state = restarted.get_log_state().await.unwrap();
            assert_eq!(state.last_log_id, Some(log_id(1, 1)));
        });
    }

    #[test]
    fn a_vote_is_durable_when_save_vote_returns_with_every_earlier_write() {
        SimRuntime::with_seed(3).block_on(async {
            let mut log = Log::new(delayed());
            log.append([blank(1, 1)], IOFlushed::noop()).await.unwrap();
            let vote = OrVote::new(2, 7);
            log.save_vote(&vote).await.unwrap();

            let survived = log.crash();
            assert_eq!(survived.vote, Some(vote));
            assert_eq!(indexes(&survived), [1], "writes are serialized");
        });
    }

    #[test]
    fn durability_follows_the_write_order() {
        // Truncate after an append, then crash between the two flushes: the
        // durable log is the result of a prefix of the writes.
        SimRuntime::with_seed(4).block_on(async {
            let mut log = Log::new(LogConfig::default());
            log.append([blank(1, 1), blank(1, 2), blank(1, 3)], IOFlushed::noop())
                .await
                .unwrap();
            let mut delayed_log = Log::from_image(log.crash(), delayed());
            delayed_log
                .truncate_after(Some(log_id(1, 1)))
                .await
                .unwrap();
            delayed_log
                .append([blank(2, 2)], IOFlushed::noop())
                .await
                .unwrap();
            delayed_log.purge(log_id(1, 1)).await.unwrap();
            assert_eq!(indexes(&delayed_log.live()), [2]);

            let survived = delayed_log.crash();
            assert_eq!(
                indexes(&survived),
                [1, 2, 3],
                "nothing after the crash point"
            );
            assert_eq!(survived.entries[&2].log_id, log_id(1, 2));
        });
    }

    #[test]
    fn purge_and_truncate_shape_the_log_state() {
        SimRuntime::with_seed(5).block_on(async {
            let mut log = Log::new(LogConfig::default());
            let entries: Vec<_> = (1..=5).map(|i| blank(1, i)).collect();
            log.append(entries, IOFlushed::noop()).await.unwrap();
            log.purge(log_id(1, 2)).await.unwrap();
            log.truncate_after(Some(log_id(1, 4))).await.unwrap();
            assert_eq!(indexes(&log.live()), [3, 4]);
            assert_eq!(
                indexes(&log.durable()),
                [1, 2, 3, 4, 5],
                "durable at the next flush"
            );
            log.sync();
            assert_eq!(indexes(&log.durable()), [3, 4]);

            let state = log.get_log_state().await.unwrap();
            assert_eq!(state.last_purged_log_id, Some(log_id(1, 2)));
            assert_eq!(state.last_log_id, Some(log_id(1, 4)));

            log.purge(log_id(1, 4)).await.unwrap();
            let state = log.get_log_state().await.unwrap();
            assert_eq!(
                state.last_log_id,
                Some(log_id(1, 4)),
                "the purge point when empty"
            );
        });
    }
}
