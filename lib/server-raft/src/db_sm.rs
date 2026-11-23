//! `DbStateMachine`: openraft's `RaftStateMachine` over one `engine::Db`
//! in `Mode::External` (plan 0030).
//!
//! - **Each committed entry at index `i` is one `Db::apply(i + 1, ops)`**
//!   (roadmap decision 1, shifted by one: openraft's log starts at index 0,
//!   the engine's sequences at 1). Every entry also writes the applied log id
//!   (`server_keys::APPLIED_KEY`); a membership entry writes
//!   `MEMBERSHIP_KEY` too. Blank entries apply with only the applied log
//!   id. `Db::apply` is all or nothing after a crash, so the data and
//!   openraft's applied state roll back together.
//! - **Commands carry stored keys.** `server` maps user keys to `0x01 ‖ key`
//!   before it proposes (plan 0026's layout); the state machine applies the
//!   operations as they are, as `ModelSm` does.
//! - **The applier.** `Applier::Thread` (production) calls `Db::apply` on a
//!   thread named `raft-applier`, so no async worker blocks on the engine
//!   (roadmap decision 14); `apply` awaits its batch. Each responder
//!   resolves as its entry applies. `Applier::Inline` (the simulator)
//!   applies on the caller.
//! - **A failed apply is fatal:** the state machine refuses every later
//!   call, and openraft stops the node.

use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::mpsc;
use std::thread::JoinHandle;

use engine::Db;
use futures_util::Stream;
use futures_util::StreamExt;
use openraft::AsyncRuntime;
use openraft::EntryPayload;
use openraft::RaftSnapshotBuilder;
use openraft::storage::EntryResponder;
use openraft::storage::RaftStateMachine;
use openraft::type_config::OneshotSender;
use openraft::type_config::TypeConfigExt;
use openraft_rt_tokio::TokioRuntime;
use server_keys::APPLIED_KEY;
use server_keys::MEMBERSHIP_KEY;

use crate::command::Op;
use crate::encoding::decode_stored_membership;
use crate::encoding::encode_stored_membership;
use crate::types::OrEntry;
use crate::types::OrLogId;
use crate::types::OrSnapshot;
use crate::types::OrSnapshotMeta;
use crate::types::OrStoredMembership;
use crate::types::Types;

/// The log hook of a checkpoint: make every queued log write durable.
pub type SyncHook = Arc<dyn Fn() -> Result<(), io::Error> + Send + Sync>;

/// Who calls `Db::apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applier {
    /// A thread named `raft-applier`: production.
    Thread,
    /// On the caller: the simulator, whose runtime has one thread.
    #[cfg(any(test, feature = "sim"))]
    Inline,
}

/// What `Db::apply` gets for one entry.
pub(crate) fn entry_ops(entry: &OrEntry) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let mut ops = Vec::new();
    match &entry.payload {
        EntryPayload::Blank => {}
        EntryPayload::Normal(command) => {
            for op in &command.ops {
                ops.push(match op {
                    Op::Put { key, value } => (key.clone(), Some(value.clone())),
                    Op::Delete { key } => (key.clone(), None),
                });
            }
        }
        EntryPayload::Membership(membership) => {
            let stored = OrStoredMembership::new(Some(entry.log_id), membership.clone());
            ops.push((
                MEMBERSHIP_KEY.to_vec(),
                Some(encode_stored_membership(&stored)),
            ));
        }
    }
    let applied = server_keys::encode_log_id(entry.log_id.leader_id.term, entry.log_id.index);
    ops.push((APPLIED_KEY.to_vec(), Some(applied.to_vec())));
    ops
}

/// The engine sequence of the entry at Raft index `index`: one more than
/// the index. openraft starts the log at index 0 (the initial membership),
/// and the engine's sequences start at 1, with 0 meaning "nothing
/// applied" (plan 0030, discoveries).
pub(crate) fn engine_seq(index: u64) -> u64 {
    index + 1
}

fn apply_entry(db: &Db, entry: &OrEntry) -> Result<engine::Applied, io::Error> {
    let owned = entry_ops(entry);
    let ops: Vec<engine::Op<'_>> = owned
        .iter()
        .map(|(key, value)| match value {
            Some(value) => engine::Op::Put { key, value },
            None => engine::Op::Delete { key },
        })
        .collect();
    db.apply(engine_seq(entry.log_id.index), &ops)
        .map_err(io::Error::other)
}

/// The applied log id and membership that `db`'s system keys hold.
pub(crate) fn applied_state_of(
    db: &Db,
) -> Result<(Option<OrLogId>, OrStoredMembership), io::Error> {
    let applied = db
        .get(APPLIED_KEY)
        .map_err(io::Error::other)?
        .map(|bytes| {
            server_keys::decode_log_id(&bytes)
                .map(|(term, index)| OrLogId::new_term_index(term, index))
                .ok_or_else(|| io::Error::other("a bad applied log id"))
        })
        .transpose()?;
    let membership = db
        .get(MEMBERSHIP_KEY)
        .map_err(io::Error::other)?
        .map(|bytes| decode_stored_membership(&bytes))
        .transpose()?
        .unwrap_or_default();
    Ok((applied, membership))
}

type Responder<RT> = openraft::storage::ApplyResponder<Types<RT>>;

/// One batch for the applier thread, and where to report its result.
struct Job<RT: AsyncRuntime> {
    entries: Vec<(OrEntry, Option<Responder<RT>>)>,
    done: openraft::type_config::alias::OneshotSenderOf<Types<RT>, Result<(), String>>,
}

/// What every handle shares.
pub(crate) struct Shared<RT: AsyncRuntime> {
    /// The database; `None` while a snapshot install replaces it.
    db: Mutex<Option<Arc<Db>>>,
    pub(crate) sync: SyncHook,
    pub(crate) applier: Applier,
    /// Held for each entry's apply, and by a snapshot build or read for
    /// its whole length, so they see entry boundaries only.
    pub(crate) gate: Mutex<()>,
    /// The metadata of the committed checkpoint, which is the current
    /// snapshot; `None` before the first apply is committed.
    pub(crate) committed: Mutex<Option<OrSnapshotMeta>>,
    /// Set after a failed apply, a failed checkpoint, or a crash.
    failed: Mutex<Option<String>>,
    jobs: Mutex<Option<mpsc::Sender<Job<RT>>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    #[cfg(any(test, feature = "sim"))]
    pub(crate) observer: Mutex<crate::sim::Observed>,
    /// The simulated store under the database, for the simulator.
    #[cfg(test)]
    pub(crate) store: Mutex<Option<engine::SimStore>>,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<RT: AsyncRuntime> Shared<RT> {
    pub(crate) fn db(&self) -> Result<Arc<Db>, io::Error> {
        self.check()?;
        lock(&self.db)
            .clone()
            .ok_or_else(|| io::Error::other("a snapshot install replaces the database"))
    }

    pub(crate) fn check(&self) -> Result<(), io::Error> {
        match &*lock(&self.failed) {
            Some(reason) => Err(io::Error::other(format!(
                "the state machine stopped: {reason}"
            ))),
            None => Ok(()),
        }
    }

    pub(crate) fn fail(&self, reason: String) {
        lock(&self.failed).get_or_insert(reason);
    }

    /// Take the database out for a snapshot install, waiting for readers
    /// that hold it to finish.
    pub(crate) fn take_db(&self) -> Result<Db, io::Error> {
        let mut db = lock(&self.db)
            .take()
            .ok_or_else(|| io::Error::other("no database to replace"))?;
        loop {
            match Arc::try_unwrap(db) {
                Ok(db) => return Ok(db),
                Err(shared) => {
                    db = shared;
                    std::thread::yield_now();
                }
            }
        }
    }

    pub(crate) fn put_db(&self, db: Db) {
        *lock(&self.db) = Some(Arc::new(db));
    }

    /// Apply a batch in order, answering each responder as its entry
    /// applies.
    fn apply_batch(&self, entries: Vec<(OrEntry, Option<Responder<RT>>)>) -> Result<(), io::Error> {
        let db = self.db()?;
        for (entry, responder) in entries {
            let applied = {
                let _gate = lock(&self.gate);
                apply_entry(&db, &entry).inspect_err(|error| self.fail(error.to_string()))?
            };
            #[cfg(any(test, feature = "sim"))]
            if applied == engine::Applied::Applied
                && let Some((node, observer)) = &*lock(&self.observer)
            {
                observer.applied(*node, &entry);
            }
            let _ = applied;
            if let Some(responder) = responder {
                responder.send(());
            }
        }
        Ok(())
    }
}

impl<RT: AsyncRuntime> Drop for Shared<RT> {
    fn drop(&mut self) {
        lock(&self.jobs).take();
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

/// openraft's state machine over an external-mode `Db`. Clones share one
/// state machine.
pub struct DbStateMachine<RT: AsyncRuntime = TokioRuntime> {
    pub(crate) shared: Arc<Shared<RT>>,
    _rt: PhantomData<fn() -> RT>,
}

impl<RT: AsyncRuntime> Clone for DbStateMachine<RT> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            _rt: PhantomData,
        }
    }
}

impl<RT: AsyncRuntime> fmt::Debug for DbStateMachine<RT> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DbStateMachine")
            .field("applier", &self.shared.applier)
            .finish_non_exhaustive()
    }
}

impl DbStateMachine<TokioRuntime> {
    /// Open an external-mode `engine::Db` at `dir` and wrap it in a
    /// production `DbStateMachine` with its dedicated applier thread.
    pub fn open_dir(
        dir: &std::path::Path,
        direct_io: bool,
        sync: SyncHook,
    ) -> Result<Self, io::Error> {
        let options = engine::Options {
            mode: engine::Mode::External,
            io: if direct_io {
                engine::Io::Direct
            } else {
                engine::Io::Cached
            },
            ..engine::Options::default()
        };
        let db = Db::open(dir, options).map_err(io::Error::other)?;
        Self::open(db, sync, Applier::Thread)
    }
}

impl<RT: AsyncRuntime> DbStateMachine<RT> {
    /// A state machine over `db`, which must be in `Mode::External`.
    /// `sync` makes the node's Raft log durable; checkpoints call it before
    /// they commit.
    pub fn open(db: Db, sync: SyncHook, applier: Applier) -> Result<Self, io::Error> {
        // Right after open, the database is at its committed checkpoint, so
        // its system keys describe the current snapshot.
        let (applied, membership) = applied_state_of(&db)?;
        let committed = applied.map(|applied| OrSnapshotMeta {
            last_log_id: Some(applied),
            last_membership: membership,
        });
        let shared = Arc::new(Shared {
            db: Mutex::new(Some(Arc::new(db))),
            sync,
            applier,
            gate: Mutex::new(()),
            committed: Mutex::new(committed),
            failed: Mutex::new(None),
            jobs: Mutex::new(None),
            thread: Mutex::new(None),
            #[cfg(any(test, feature = "sim"))]
            observer: Mutex::new(None),
            #[cfg(test)]
            store: Mutex::new(None),
        });
        if applier == Applier::Thread {
            let (tx, rx) = mpsc::channel::<Job<RT>>();
            let worker = Arc::downgrade(&shared);
            let thread = std::thread::Builder::new()
                .name("raft-applier".to_owned())
                .spawn(move || {
                    for job in rx {
                        let result = match worker.upgrade() {
                            Some(shared) => shared
                                .apply_batch(job.entries)
                                .map_err(|error| error.to_string()),
                            None => Err("the state machine was dropped".to_owned()),
                        };
                        let _ = job.done.send(result);
                    }
                })?;
            *lock(&shared.jobs) = Some(tx);
            *lock(&shared.thread) = Some(thread);
        }
        Ok(Self {
            shared,
            _rt: PhantomData,
        })
    }

    /// A state machine over an external database on a simulated page
    /// store, for the simulator: a snapshot install replaces the store,
    /// and [`DbStateMachine::sim_store`] returns the current one.
    #[cfg(test)]
    pub(crate) fn open_sim(
        store: engine::SimStore,
        options: engine::Options,
        sync: SyncHook,
        applier: Applier,
    ) -> Result<Self, io::Error> {
        let db = Db::open_external_on(store.clone(), options).map_err(io::Error::other)?;
        let sm = Self::open(db, sync, applier)?;
        *lock(&sm.shared.store) = Some(store);
        Ok(sm)
    }

    /// The simulated store under the database now.
    #[cfg(test)]
    pub(crate) fn sim_store(&self) -> Option<engine::SimStore> {
        lock(&self.shared.store).clone()
    }

    /// Crash: return what the simulated store keeps, tearing unsynced
    /// writes with `seed`, and stop the store and the state machine.
    #[cfg(test)]
    pub(crate) fn crash(&self, seed: u64) -> engine::SimImage {
        self.shared.fail("crashed".to_owned());
        let store = self.sim_store().expect("a simulated store");
        let image = store.crash(seed);
        store.kill();
        image
    }

    /// The last log id of the durable snapshot: the committed checkpoint.
    #[cfg(test)]
    pub(crate) fn durable_log_id(&self) -> Option<OrLogId> {
        lock(&self.shared.committed)
            .as_ref()
            .and_then(|meta| meta.last_log_id)
    }

    /// Report this state machine's applies to `observer` as `node`'s.
    #[cfg(test)]
    pub(crate) fn observe(
        &self,
        node: crate::types::NodeId,
        observer: Arc<dyn crate::sim::Observer>,
    ) {
        *lock(&self.shared.observer) = Some((node, observer));
    }

    /// The database, for reads after a read barrier (S7's `Kv` service).
    pub fn db(&self) -> Result<Arc<Db>, io::Error> {
        self.shared.db()
    }

    /// The value of a stored key.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.db().ok()?.get(key).ok()?
    }

    /// The user keyspace's contents.
    pub fn user_contents(&self) -> std::collections::BTreeMap<Vec<u8>, Vec<u8>> {
        let Ok(db) = self.db() else {
            return Default::default();
        };
        db.range(Some(&[server_keys::USER]), None)
            .map(|scan| scan.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }
}

impl<RT: AsyncRuntime> RaftStateMachine<Types<RT>> for DbStateMachine<RT> {
    type SnapshotData = Vec<u8>;
    type SnapshotBuilder = Self;

    async fn applied_state(&mut self) -> Result<(Option<OrLogId>, OrStoredMembership), io::Error> {
        applied_state_of(self.shared.db()?.as_ref())
    }

    async fn apply<S>(&mut self, mut entries: S) -> Result<(), io::Error>
    where
        S: Stream<Item = Result<EntryResponder<Types<RT>>, io::Error>> + Unpin + Send,
    {
        let mut batch = Vec::new();
        while let Some(item) = entries.next().await {
            batch.push(item?);
        }
        match self.shared.applier {
            #[cfg(any(test, feature = "sim"))]
            Applier::Inline => self.shared.apply_batch(batch),
            Applier::Thread => {
                let (done, rx) = Types::<RT>::oneshot();
                let sent = lock(&self.shared.jobs).as_ref().map(|jobs| {
                    jobs.send(Job {
                        entries: batch,
                        done,
                    })
                });
                if !matches!(sent, Some(Ok(()))) {
                    return Err(io::Error::other("the applier thread stopped"));
                }
                rx.await
                    .map_err(|_| io::Error::other("the applier thread stopped"))?
                    .map_err(io::Error::other)
            }
        }
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn install_snapshot(
        &mut self,
        meta: &OrSnapshotMeta,
        snapshot: Vec<u8>,
    ) -> Result<(), io::Error> {
        crate::db_snapshot::install(&self.shared, meta.clone(), snapshot).await
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<OrSnapshot>, io::Error> {
        crate::db_snapshot::current(&self.shared).await
    }
}

impl<RT: AsyncRuntime> RaftSnapshotBuilder<Types<RT>> for DbStateMachine<RT> {
    type SnapshotData = Vec<u8>;

    async fn build_snapshot(&mut self) -> Result<OrSnapshot, io::Error> {
        crate::db_snapshot::build(&self.shared).await
    }
}
