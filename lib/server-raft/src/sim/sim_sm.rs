//! `SimSm`: the state machine a simulated node runs (plan 0030, diff 9).
//!
//! By default the real one, `DbStateMachine` over an external-mode `Db` on
//! a `SimStore`: a crash keeps what the simulated store durably holds,
//! with torn writes, and a restart reopens the engine from it. `ModelSm`
//! stays as the reference model (`SmKind::Model`).

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use engine::Db;
use engine::Mode;
use engine::Options;
use engine::SimImage;
use engine::SimStore;
use engine::TreeConfig;
use futures_util::Stream;
use openraft::RaftSnapshotBuilder;
use openraft::storage::EntryResponder;
use openraft::storage::RaftStateMachine;

use super::ModelSm;
use super::Observer;
use super::StoredSnapshot;
use super::simulator::Rt;
use crate::db_sm::Applier;
use crate::db_sm::DbStateMachine;
use crate::db_sm::SyncHook;
use crate::types::NodeId;
use crate::types::OrLogId;
use crate::types::OrSnapshot;
use crate::types::OrSnapshotMeta;
use crate::types::OrStoredMembership;
use crate::types::Types;

/// Which state machine the simulated nodes run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmKind {
    /// `DbStateMachine` over a `SimStore`.
    Db,
    /// The `ModelSm` model.
    Model,
}

/// What survives a node's crash.
#[derive(Debug, Clone)]
pub enum SmDurable {
    Model(Option<StoredSnapshot>),
    Db(SimImage),
}

/// The engine's settings in the simulator: a small buffer, so pages reach
/// the store and crashes tear them.
pub fn engine_options() -> Options {
    Options {
        mode: Mode::External,
        tree: TreeConfig {
            circular_buffer_bytes: 256 << 10,
            ..TreeConfig::default()
        },
        ..Options::default()
    }
}

/// A simulated node's state machine. Clones share one.
#[derive(Clone)]
pub enum SimSm {
    Model(ModelSm<Rt>),
    Db(DbStateMachine<Rt>),
}

impl SimSm {
    /// A state machine restarted from `durable`, or an empty one of `kind`.
    /// `sync` makes the node's log durable.
    pub fn start(kind: SmKind, durable: Option<&SmDurable>, sync: SyncHook) -> Self {
        match (kind, durable) {
            (SmKind::Model, None) | (_, Some(SmDurable::Model(_))) => {
                let snapshot = match durable {
                    Some(SmDurable::Model(snapshot)) => snapshot.clone(),
                    _ => None,
                };
                SimSm::Model(ModelSm::from_durable(
                    snapshot,
                    Arc::new(move || drop(sync())),
                ))
            }
            (SmKind::Db, None) | (_, Some(SmDurable::Db(_))) => {
                let store = match durable {
                    Some(SmDurable::Db(image)) => SimStore::from_image(image),
                    _ => Db::new_sim_store(&engine_options()).expect("a simulated store"),
                };
                let sm = DbStateMachine::open_sim(store, engine_options(), sync, Applier::Inline)
                    .unwrap_or_else(|error| panic!("the state machine does not recover: {error}"));
                SimSm::Db(sm)
            }
        }
    }

    pub(crate) fn observe(&self, node: NodeId, observer: Arc<dyn Observer>) {
        match self {
            SimSm::Model(sm) => sm.observe(node, observer),
            SimSm::Db(sm) => sm.observe(node, observer),
        }
    }

    /// The value of a stored key.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        match self {
            SimSm::Model(sm) => sm.get(key),
            SimSm::Db(sm) => sm.get(key),
        }
    }

    /// The user keyspace's contents.
    pub fn user_contents(&self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        match self {
            SimSm::Model(sm) => sm.user_contents(),
            SimSm::Db(sm) => sm.user_contents(),
        }
    }

    /// The last log id of the durable snapshot.
    pub fn durable_log_id(&self) -> Option<OrLogId> {
        match self {
            SimSm::Model(sm) => sm.durable().and_then(|snapshot| snapshot.meta.last_log_id),
            SimSm::Db(sm) => sm.durable_log_id(),
        }
    }

    /// Crash: lose what is not durable, tearing unsynced writes with
    /// `seed`, and stop.
    pub fn crash(&self, seed: u64) -> SmDurable {
        match self {
            SimSm::Model(sm) => SmDurable::Model(sm.crash()),
            SimSm::Db(sm) => SmDurable::Db(sm.crash(seed)),
        }
    }
}

impl RaftSnapshotBuilder<Types<Rt>> for SimSm {
    type SnapshotData = Vec<u8>;

    async fn build_snapshot(&mut self) -> Result<OrSnapshot, io::Error> {
        match self {
            SimSm::Model(sm) => sm.build_snapshot().await,
            SimSm::Db(sm) => sm.build_snapshot().await,
        }
    }
}

impl RaftStateMachine<Types<Rt>> for SimSm {
    type SnapshotData = Vec<u8>;
    type SnapshotBuilder = Self;

    async fn applied_state(&mut self) -> Result<(Option<OrLogId>, OrStoredMembership), io::Error> {
        match self {
            SimSm::Model(sm) => sm.applied_state().await,
            SimSm::Db(sm) => sm.applied_state().await,
        }
    }

    async fn apply<S>(&mut self, entries: S) -> Result<(), io::Error>
    where
        S: Stream<Item = Result<EntryResponder<Types<Rt>>, io::Error>> + Unpin + Send,
    {
        match self {
            SimSm::Model(sm) => sm.apply(entries).await,
            SimSm::Db(sm) => sm.apply(entries).await,
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
        match self {
            SimSm::Model(sm) => sm.install_snapshot(meta, snapshot).await,
            SimSm::Db(sm) => sm.install_snapshot(meta, snapshot).await,
        }
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<OrSnapshot>, io::Error> {
        match self {
            SimSm::Model(sm) => sm.get_current_snapshot().await,
            SimSm::Db(sm) => sm.get_current_snapshot().await,
        }
    }
}
