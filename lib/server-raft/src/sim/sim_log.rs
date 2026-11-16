//! `SimLog`: the log a simulated node runs over (plan 0029, diff 8).
//!
//! By default the real on-disk log, `DiskLog` over `SimFs`: a crash keeps
//! what the simulated file system durably holds, with torn tails, and a
//! restart recovers from it with the real recovery code. `MemLog` stays as
//! the reference model, and runs the deliberate lazy-vote bug.

use std::fmt::Debug;
use std::io;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Arc;

use openraft::LogState;
use openraft::RaftLogReader;
use openraft::storage::IOFlushed;
use openraft::storage::RaftLogStorage;
use server_raft_log::LogOptions;
use server_raft_log::SimFs;
use server_raft_log::SimImage;

use super::LogConfig;
use super::LogImage;
use super::MemLog;
use super::Observer;
use super::simulator::Rt;
use crate::disk_log::DiskLog;
use crate::disk_log::Flusher;
use crate::types::NodeId;
use crate::types::OrEntry;
use crate::types::OrLogId;
use crate::types::OrVote;
use crate::types::Types;

/// Which log the simulated nodes run over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// `DiskLog` over `SimFs`.
    Disk,
    /// The `MemLog` model.
    Mem,
}

/// What survives a node's crash.
#[derive(Debug, Clone)]
pub enum Durable {
    Mem(LogImage),
    Disk(SimImage),
}

fn dir() -> &'static Path {
    Path::new("/raft-log")
}

/// Small segments, so runs roll, truncate, and purge across segments.
fn options() -> LogOptions {
    LogOptions {
        segment_bytes: 4 << 10,
        cache_bytes: 16 << 10,
    }
}

/// A simulated node's log. Clones share one log.
#[derive(Clone)]
pub enum SimLog {
    Mem(MemLog<Rt>),
    Disk { log: DiskLog<Rt, SimFs>, fs: SimFs },
}

impl SimLog {
    /// A log restarted from `durable`, or an empty one of `kind`.
    pub fn start(kind: LogKind, durable: Option<&Durable>, config: LogConfig) -> Self {
        match (kind, durable) {
            (LogKind::Mem, None) => SimLog::Mem(MemLog::new(config)),
            (_, Some(Durable::Mem(image))) => {
                SimLog::Mem(MemLog::from_image(image.clone(), config))
            }
            (LogKind::Disk, None) => Self::disk(SimFs::new(), &config),
            (_, Some(Durable::Disk(image))) => Self::disk(SimFs::from_image(image), &config),
        }
    }

    fn disk(fs: SimFs, config: &LogConfig) -> Self {
        let flusher = Flusher::Task {
            max_delay: config.max_flush_delay,
            seed: config.seed,
        };
        let log = DiskLog::open(fs.clone(), dir(), options(), flusher)
            .unwrap_or_else(|error| panic!("the Raft log does not recover: {error}"));
        SimLog::Disk { log, fs }
    }

    pub(crate) fn observe(&self, node: NodeId, observer: Arc<dyn Observer>) {
        match self {
            SimLog::Mem(log) => log.observe(node, observer),
            SimLog::Disk { log, .. } => log.observe(node, observer),
        }
    }

    /// What readers see now.
    pub fn live(&self) -> LogImage {
        match self {
            SimLog::Mem(log) => log.live(),
            SimLog::Disk { log, .. } => log.live(),
        }
    }

    /// What a restart from `durable` recovers.
    pub fn image(durable: &Durable) -> LogImage {
        match durable {
            Durable::Mem(image) => image.clone(),
            Durable::Disk(_) => {
                Self::start(LogKind::Disk, Some(durable), LogConfig::default()).live()
            }
        }
    }

    pub fn durable_vote(&self) -> Option<OrVote> {
        match self {
            SimLog::Mem(log) => log.durable().vote,
            SimLog::Disk { log, .. } => log.durable_vote(),
        }
    }

    pub fn is_durable(&self, log_id: OrLogId) -> bool {
        match self {
            SimLog::Mem(log) => log.is_durable(log_id),
            SimLog::Disk { log, .. } => log.is_durable(log_id),
        }
    }

    /// Make every queued write durable: the state machine's checkpoint
    /// hook.
    pub fn sync(&self) {
        match self {
            SimLog::Mem(log) => log.sync(),
            // A failure stops the node; the checkpoint proceeds, as its
            // entries are committed on a quorum anyway.
            SimLog::Disk { log, .. } => drop(log.sync()),
        }
    }

    /// Crash: lose what is not durable, tearing unsynced tails with
    /// `seed`, and freeze the log.
    pub fn crash(&self, seed: u64) -> Durable {
        match self {
            SimLog::Mem(log) => Durable::Mem(log.crash()),
            SimLog::Disk { fs, .. } => {
                let image = fs.crash(seed);
                // Power stays off: every later write fails.
                fs.cut_power_at(Some(fs.ops()));
                Durable::Disk(image)
            }
        }
    }

    /// Make the next file-system operation fail, as a device error would.
    /// Does nothing for `MemLog`.
    pub fn fail_next_operation(&self) {
        if let SimLog::Disk { fs, .. } = self {
            fs.fail_op(Some(fs.ops()));
        }
    }

    /// Whether the log failed and refuses writes, so its node stopped.
    pub fn is_failed(&self) -> bool {
        match self {
            SimLog::Mem(_) => false,
            SimLog::Disk { log, .. } => log.is_failed(),
        }
    }
}

impl RaftLogReader<Types<Rt>> for SimLog {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<OrEntry>, io::Error> {
        match self {
            SimLog::Mem(log) => log.try_get_log_entries(range).await,
            SimLog::Disk { log, .. } => log.try_get_log_entries(range).await,
        }
    }

    async fn read_vote(&mut self) -> Result<Option<OrVote>, io::Error> {
        match self {
            SimLog::Mem(log) => log.read_vote().await,
            SimLog::Disk { log, .. } => log.read_vote().await,
        }
    }
}

impl RaftLogStorage<Types<Rt>> for SimLog {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<Types<Rt>>, io::Error> {
        match self {
            SimLog::Mem(log) => log.get_log_state().await,
            SimLog::Disk { log, .. } => log.get_log_state().await,
        }
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &OrVote) -> Result<(), io::Error> {
        match self {
            SimLog::Mem(log) => log.save_vote(vote).await,
            SimLog::Disk { log, .. } => log.save_vote(vote).await,
        }
    }

    async fn save_committed(&mut self, committed: Option<OrLogId>) -> Result<(), io::Error> {
        match self {
            SimLog::Mem(log) => log.save_committed(committed).await,
            SimLog::Disk { log, .. } => log.save_committed(committed).await,
        }
    }

    async fn read_committed(&mut self) -> Result<Option<OrLogId>, io::Error> {
        match self {
            SimLog::Mem(log) => log.read_committed().await,
            SimLog::Disk { log, .. } => log.read_committed().await,
        }
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: IOFlushed<Types<Rt>>,
    ) -> Result<(), io::Error>
    where
        I: IntoIterator<Item = OrEntry> + Send,
        I::IntoIter: Send,
    {
        match self {
            SimLog::Mem(log) => log.append(entries, callback).await,
            SimLog::Disk { log, .. } => log.append(entries, callback).await,
        }
    }

    async fn truncate_after(&mut self, last_log_id: Option<OrLogId>) -> Result<(), io::Error> {
        match self {
            SimLog::Mem(log) => log.truncate_after(last_log_id).await,
            SimLog::Disk { log, .. } => log.truncate_after(last_log_id).await,
        }
    }

    async fn purge(&mut self, log_id: OrLogId) -> Result<(), io::Error> {
        match self {
            SimLog::Mem(log) => log.purge(log_id).await,
            SimLog::Disk { log, .. } => log.purge(log_id).await,
        }
    }
}
