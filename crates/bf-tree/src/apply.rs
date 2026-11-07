//! Writes that a log sequences (plan 0019): entries applied from a log, and
//! single writes whose sequence a log assigns.

use super::{Tree, TreeError};
use bf_tree_page::RecordState;
use bf_tree_sync::atomic::Ordering;

/// Where a tree's sequences come from, latched on first use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Sequencing {
    Unset = 0,
    /// `put` and `remove`: the tree's own counter.
    Internal = 1,
    /// `apply` and the logged writes: a log.
    External = 2,
}

/// One write. Payloads borrow from the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op<'a> {
    Put { key: &'a [u8], value: &'a [u8] },
    Delete { key: &'a [u8] },
}

impl<'a> Op<'a> {
    pub fn key(&self) -> &'a [u8] {
        match *self {
            Op::Put { key, .. } | Op::Delete { key } => key,
        }
    }

    fn state(&self) -> RecordState<'a> {
        match *self {
            Op::Put { value, .. } => RecordState::Insert(value),
            Op::Delete { .. } => RecordState::Delete,
        }
    }
}

/// One sequenced unit: a WAL record or a replicated log entry. `ops` may be
/// empty (for example, a replicated no-op entry); it still advances the
/// applied watermark.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    pub seq: u64,
    pub ops: &'a [Op<'a>],
}

/// Assigns sequences to writes as the tree applies them.
///
/// `append` runs with the leaf locked and the route validated, before the
/// record is applied. It must return strictly increasing sequences in real
/// time across all callers. It must not wait for durability and must not
/// call into the tree. If the tree must retry the write, for example after
/// making room in the circular buffer, it calls `append` again for the same
/// `op`. The log then holds an earlier record that no applied write used;
/// replaying both records is harmless.
pub trait Log: Send + Sync {
    fn append(&self, op: Op<'_>) -> Result<u64, LogError>;
}

/// A log failure, distinct from store failures. `Copy`, like `TreeError`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LogError {
    #[error("log is closed")]
    Closed,
    #[error("log I/O error during {op}: {kind}")]
    Io {
        op: &'static str,
        kind: std::io::ErrorKind,
        code: Option<i32>,
    },
    #[error("log failed: {0}")]
    Other(&'static str),
}

impl Tree {
    /// The key and value rules every write must pass. Public so that a log
    /// owner can reject an operation before logging it.
    pub fn check_op(&self, op: &Op<'_>) -> Result<(), TreeError> {
        self.check_key(op.key())?;
        if let Op::Put { value, .. } = op
            && value.len() > self.config.max_value_len
        {
            return Err(TreeError::ValueTooLarge);
        }
        Ok(())
    }

    /// Apply one sequenced entry: every operation with `entry.seq`, in
    /// order. Every operation is checked first, so an invalid one rejects
    /// the whole entry unchanged. The applied watermark advances once,
    /// after the last operation, so a checkpoint never records a sequence
    /// inside an entry.
    ///
    /// Entries must come from one caller at a time, in increasing sequence
    /// order. A store error after the entry started applying can leave it
    /// partly applied; the caller must then reopen and replay.
    pub fn apply(&self, entry: Entry<'_>) -> Result<(), TreeError> {
        entry.ops.iter().try_for_each(|op| self.check_op(op))?;
        self.accept_external(entry.seq)?;
        for op in entry.ops {
            self.write(op.key(), op.state(), &mut || Ok(entry.seq))?;
        }
        self.applied.fetch_max(entry.seq, Ordering::Release);
        Ok(())
    }

    /// Insert or replace `key` with a sequence that `log` assigns under the
    /// leaf lock. Returns the sequence.
    pub fn put_logged(&self, log: &dyn Log, key: &[u8], value: &[u8]) -> Result<u64, TreeError> {
        self.write_logged(log, Op::Put { key, value })
    }

    /// Remove `key` with a sequence that `log` assigns under the leaf lock.
    /// Returns the sequence.
    pub fn remove_logged(&self, log: &dyn Log, key: &[u8]) -> Result<u64, TreeError> {
        self.write_logged(log, Op::Delete { key })
    }

    fn write_logged(&self, log: &dyn Log, op: Op<'_>) -> Result<u64, TreeError> {
        self.check_op(&op)?;
        self.claim_external()?;
        let seq = self.write(op.key(), op.state(), &mut || {
            let seq = log.append(op).map_err(TreeError::Log)?;
            // Before the effect, so a checkpoint's captured sequence covers it.
            self.last_seq.fetch_max(seq, Ordering::AcqRel);
            Ok(seq)
        })?;
        // After the leaf lock is released.
        self.applied.fetch_max(seq, Ordering::Release);
        Ok(seq)
    }

    /// Accept an entry's sequence: it must be above the applied watermark,
    /// because re-applying an old entry after newer ones would overwrite
    /// newer values. A failed entry is not marked applied, so a retry of it
    /// is accepted.
    fn accept_external(&self, seq: u64) -> Result<(), TreeError> {
        self.claim_external()?;
        let applied = self.applied.load(Ordering::Acquire);
        if seq <= applied {
            return Err(TreeError::SequenceOrder { seq, applied });
        }
        self.last_seq.fetch_max(seq, Ordering::AcqRel);
        Ok(())
    }

    fn claim_external(&self) -> Result<(), TreeError> {
        self.claim(Sequencing::External)
    }

    /// Latch the tree's sequence source on first use. Mixing sources in one
    /// process is unsafe: if a log has applied up to 10 and `put` then takes
    /// 11, a checkpoint can record 11 before the log's own entry 11 arrives,
    /// and replay from 12 would skip it. The latch resets on open.
    pub(super) fn claim(&self, wanted: Sequencing) -> Result<(), TreeError> {
        let wanted = wanted as u8;
        match self.sequencing.compare_exchange(
            Sequencing::Unset as u8,
            wanted,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(current) if current == wanted => Ok(()),
            Err(_) => Err(TreeError::MixedSequencing),
        }
    }
}
