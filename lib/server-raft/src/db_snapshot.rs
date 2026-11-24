//! Snapshots of `DbStateMachine` (plans 0030, 0034).
//!
//! A snapshot is the engine's committed checkpoint (roadmap decision 3):
//!
//! - **Build** (openraft's snapshot policy, which is compaction): with the
//!   applier paused at an entry boundary, `checkpoint_with` commits with
//!   the log's sync hook (decision 2; owner obligation 4), the system keys
//!   give the metadata, and `write_snapshot` streams the committed image
//!   directly into a temporary [`SnapshotFile`] (on disk) or `Vec<u8>` (in
//!   the simulator). openraft then purges the log below it.
//! - **Install:** sync the log first, then `Db::install_snapshot` streaming
//!   directly from disk (or slice in the simulator), and check that the
//!   installed image's system keys match the metadata. The sync must come
//!   first: openraft queues the truncation of conflicting entries before it
//!   installs, and a crash that kept the snapshot but lost the truncation
//!   leaves a log openraft refuses to start over (plan 0029's discovery).
//! - **Current:** the committed checkpoint is the current snapshot; its
//!   metadata is kept, and its image is streamed on demand.
//!
//! The engine calls block, so with the applier thread they run on the
//! blocking pool; in the simulator they run inline.

use std::io::{self, BufReader};
use std::sync::Arc;

use openraft::AsyncRuntime;

use crate::db_sm::Applier;
use crate::db_sm::Shared;
use crate::db_sm::applied_state_of;
use crate::db_sm::lock;
use crate::snapshot_file::SnapshotData;
use crate::snapshot_file::SnapshotFile;
use crate::types::OrDbSnapshot;
use crate::types::OrSnapshotMeta;

/// Run `work` off the async threads with the applier thread, inline in the
/// simulator.
async fn blocking<RT: AsyncRuntime, T: Send + 'static>(
    shared: &Arc<Shared<RT>>,
    work: impl FnOnce(&Shared<RT>) -> Result<T, io::Error> + Send + 'static,
) -> Result<T, io::Error> {
    match shared.applier {
        Applier::Thread => {
            let shared = Arc::clone(shared);
            RT::spawn_blocking(move || work(&shared)).await?
        }
        #[cfg(any(test, feature = "sim"))]
        Applier::Inline => work(shared),
    }
}

fn export_snapshot<RT: AsyncRuntime>(
    shared: &Shared<RT>,
    db: &engine::Db,
) -> Result<SnapshotData, io::Error> {
    match &shared.snapshot_dir {
        Some(dir) => {
            let file = SnapshotFile::create_in(dir, |w| {
                db.write_snapshot(w).map(|_| ()).map_err(io::Error::other)
            })?;
            Ok(SnapshotData::File(file))
        }
        None => {
            let mut data = Vec::new();
            db.write_snapshot(&mut data).map_err(io::Error::other)?;
            Ok(SnapshotData::Bytes(data))
        }
    }
}

pub(crate) async fn build<RT: AsyncRuntime>(
    shared: &Arc<Shared<RT>>,
) -> Result<OrDbSnapshot, io::Error> {
    blocking(shared, |shared| {
        let db = shared.db()?;
        let _paused = lock(&shared.gate);
        let mut committed = lock(&shared.committed);
        let sync = Arc::clone(&shared.sync);
        let ckpt = db
            .checkpoint_with(|_captured| sync().map_err(Into::into))
            .map_err(io::Error::other)?;
        *lock(&shared.last_checkpoint) = Some(ckpt);
        let (applied, membership) = applied_state_of(&db)?;
        let meta = OrSnapshotMeta {
            last_log_id: applied,
            last_membership: membership,
        };
        let snapshot = export_snapshot(shared, &db)?;
        if meta.last_log_id.is_some() {
            *committed = Some(meta.clone());
        }
        Ok(OrDbSnapshot { meta, snapshot })
    })
    .await
}

pub(crate) async fn install<RT: AsyncRuntime>(
    shared: &Arc<Shared<RT>>,
    meta: OrSnapshotMeta,
    snapshot: SnapshotData,
) -> Result<(), io::Error> {
    blocking(shared, move |shared| {
        shared.check()?;
        let _paused = lock(&shared.gate);
        let mut committed = lock(&shared.committed);
        (shared.sync)()?;
        let db = shared.take_db()?;
        let installed = install_db(shared, db, &snapshot).inspect_err(|error| {
            shared.fail(format!("a snapshot install failed: {error}"));
        })?;
        let (applied, membership) = applied_state_of(&installed)?;
        shared.put_db(installed);
        if applied != meta.last_log_id || membership != meta.last_membership {
            let reason = "an installed snapshot's system keys do not match its metadata";
            shared.fail(reason.to_owned());
            return Err(io::Error::other(reason));
        }
        *committed = Some(meta);
        Ok(())
    })
    .await
}

/// Replace `db` with the snapshot: on a simulated store, a new store; on disk,
/// streaming directly from `SnapshotFile` without loading the image into RAM.
fn install_db<RT: AsyncRuntime>(
    shared: &Shared<RT>,
    db: engine::Db,
    snapshot: &SnapshotData,
) -> Result<engine::Db, io::Error> {
    #[cfg(test)]
    if lock(&shared.store).is_some() {
        let mut reader = snapshot.open_reader_at(0)?;
        let (db, store) = db
            .install_snapshot_on(&mut reader)
            .map_err(io::Error::other)?;
        *lock(&shared.store) = Some(store);
        return Ok(db);
    }
    let _ = shared;
    match snapshot {
        SnapshotData::Bytes(bytes) => db
            .install_snapshot(&mut &bytes[..])
            .map_err(io::Error::other),
        SnapshotData::File(file) => {
            let mut reader = BufReader::new(file.open_at(0)?);
            db.install_snapshot(&mut reader).map_err(io::Error::other)
        }
    }
}

pub(crate) async fn current<RT: AsyncRuntime>(
    shared: &Arc<Shared<RT>>,
) -> Result<Option<OrDbSnapshot>, io::Error> {
    blocking(shared, |shared| {
        let db = shared.db()?;
        let committed = lock(&shared.committed);
        let Some(meta) = committed.clone() else {
            return Ok(None);
        };
        let snapshot = export_snapshot(shared, &db)?;
        Ok(Some(OrDbSnapshot { meta, snapshot }))
    })
    .await
}
