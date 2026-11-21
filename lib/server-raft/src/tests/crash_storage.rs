//! The log and the state machine crashed together (plan 0030, diff 8).
//!
//! A driver uses `DiskLog` over `SimFs` and `DbStateMachine` over
//! `SimStore` as openraft does: it appends, makes the log durable, applies
//! committed entries, builds snapshots, and purges the log below them; a
//! second script installs a leader's snapshot over conflicting entries. A
//! crash cuts one disk at every operation in turn (the driver stops at the
//! first error), then crashes both, tearing their unsynced writes.
//!
//! After each crash the recovered storage must:
//!
//! - hold every entry reported durable, in the log or in the state machine;
//! - have a state machine at or beyond the log's purge point, with
//!   contents equal to the entries applied up to its applied log id;
//! - catch up by redelivering the log above it; and
//! - start an openraft node, which checks the log and the state machine
//!   against each other.
//!
//! A negative control replaces the sync hook with one that does nothing:
//! the install script must then fail the checks. Compaction passes even
//! without the hook: the applier is paused while a checkpoint commits, so
//! the checkpoint never holds part of an entry, and a state machine ahead
//! of its own log is allowed (plan 0030, discoveries). The hook stays, as
//! the engine's owner contract asks.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engine::Db;
use engine::SimStore;
use futures_util::stream;
use openraft::AsyncRuntime;
use openraft::LogState;
use openraft::RaftLogReader;
use openraft::RaftSnapshotBuilder;
use openraft::storage::IOFlushed;
use openraft::storage::RaftLogStorage;
use openraft::storage::RaftStateMachine;
use openraft_rt_sim::SimRuntime;
use server_raft_log::LogOptions;
use server_raft_log::SimFs;
use server_raft_log::SimImage;

use super::db_sm::options;
use super::db_sm::put;
use crate::db_sm::Applier;
use crate::db_sm::DbStateMachine;
use crate::db_sm::SyncHook;
use crate::disk_log::DiskLog;
use crate::disk_log::Flusher;
use crate::node::RaftConfig;
use crate::node::RaftNode;
use crate::sim::Router;
use crate::types::OrEntry;
use crate::types::OrLogId;
use crate::types::log_id;

type Rt = SimRuntime;
type Log = DiskLog<Rt, SimFs>;
type Sm = DbStateMachine<Rt>;

fn dir() -> &'static Path {
    Path::new("/raft-log")
}

/// Small segments, so purges delete files.
fn log_options() -> LogOptions {
    LogOptions {
        segment_bytes: 1 << 10,
        cache_bytes: 4 << 10,
    }
}

/// Flushes only through `sync`: the simulated clock never reaches the
/// delay inside a script.
fn flusher() -> Flusher {
    Flusher::Task {
        max_delay: Duration::from_secs(3600),
        seed: 1,
    }
}

fn entry(term: u64, index: u64) -> OrEntry {
    put(
        term,
        index,
        &[0x01, (index % 7) as u8],
        &index.to_le_bytes(),
    )
}

/// Storage of one node, and the hook its state machine uses.
struct Node {
    fs: SimFs,
    log: Log,
    sm: Sm,
}

fn real_hook(log: &Log) -> SyncHook {
    let log = log.clone();
    Arc::new(move || log.sync())
}

fn broken_hook(_log: &Log) -> SyncHook {
    Arc::new(|| Ok(()))
}

type Hook = fn(&Log) -> SyncHook;

impl Node {
    fn new(hook: Hook) -> Self {
        let fs = SimFs::new();
        let log = DiskLog::open(fs.clone(), dir(), log_options(), flusher()).unwrap();
        let store = Db::new_sim_store(&options()).unwrap();
        let sm = Sm::open_sim(store, options(), hook(&log), Applier::Inline).unwrap();
        Self { fs, log, sm }
    }

    fn store(&self) -> SimStore {
        self.sm.sim_store().unwrap()
    }

    /// Operations on each disk so far.
    fn ops(&self) -> (u64, u64) {
        (self.fs.ops(), self.store().calls())
    }

    fn crash(&self, seed: u64) -> (SimImage, engine::SimImage) {
        let log = self.fs.crash(seed);
        let store = self.store().crash(seed ^ 0x5eed);
        self.fs.cut_power_at(Some(self.fs.ops()));
        self.store().kill();
        (log, store)
    }
}

/// What a script reports.
#[derive(Default)]
struct Outcome {
    /// Every entry through this index was reported durable.
    durable: Option<u64>,
    /// Whether a snapshot install returned.
    installed: Option<OrLogId>,
}

async fn apply(sm: &mut Sm, entries: Vec<OrEntry>) -> Result<(), std::io::Error> {
    sm.apply(stream::iter(entries.into_iter().map(|e| Ok((e, None)))))
        .await
}

/// Append, sync, apply, snapshot, and purge, as a leader does.
async fn compaction_script(node: &mut Node, outcome: &mut Outcome) -> Result<(), std::io::Error> {
    let mut next = 0;
    for round in 0..8u64 {
        let batch: Vec<_> = (next..next + 5).map(|i| entry(1, i)).collect();
        next += 5;
        node.log.append(batch.clone(), IOFlushed::noop()).await?;
        node.log.sync()?;
        outcome.durable = Some(next - 1);
        node.log.save_committed(Some(log_id(1, next - 1))).await?;
        apply(&mut node.sm, batch).await?;
        if round % 3 == 2 {
            let snapshot = node.sm.build_snapshot().await?;
            if let Some(last) = snapshot.meta.last_log_id
                && last.index > 4
            {
                node.log.purge(log_id(1, last.index - 4)).await?;
            }
        }
    }
    // Applied but not yet durable in the log: a snapshot must make them
    // durable before it commits (owner obligation 4).
    let batch: Vec<_> = (next..next + 3).map(|i| entry(1, i)).collect();
    node.log.append(batch.clone(), IOFlushed::noop()).await?;
    apply(&mut node.sm, batch).await?;
    node.sm.build_snapshot().await?;
    Ok(())
}

/// The leader's snapshot at log id 2.10, for the install script.
async fn leader_snapshot() -> crate::types::OrSnapshot {
    let mut leader = Node::new(real_hook);
    let mut entries: Vec<_> = (0..=5).map(|i| entry(1, i)).collect();
    entries.extend((6..=10).map(|i| entry(2, i)));
    apply(&mut leader.sm, entries).await.unwrap();
    leader.sm.build_snapshot().await.unwrap()
}

/// A follower holds term-1 entries above a leader's snapshot: openraft
/// truncates the conflicting ones, installs the snapshot, and purges.
async fn install_script(node: &mut Node, outcome: &mut Outcome) -> Result<(), std::io::Error> {
    let snapshot = leader_snapshot().await;
    let entries: Vec<_> = (0..=12).map(|i| entry(1, i)).collect();
    node.log.append(entries.clone(), IOFlushed::noop()).await?;
    node.log.sync()?;
    outcome.durable = Some(12);
    apply(&mut node.sm, entries[..=5].to_vec()).await?;
    node.sm.build_snapshot().await?;
    node.log.truncate_after(Some(log_id(1, 5))).await?;
    // Raft removes the conflicting entries; only 0..=5 must survive.
    outcome.durable = Some(5);
    node.sm
        .install_snapshot(&snapshot.meta, snapshot.snapshot)
        .await?;
    outcome.installed = snapshot.meta.last_log_id;
    node.log.purge(log_id(2, 10)).await?;
    node.log.sync()?;
    Ok(())
}

type Run<'a> = futures_util::future::LocalBoxFuture<'a, Result<(), std::io::Error>>;
type Script = for<'a> fn(&'a mut Node, &'a mut Outcome) -> Run<'a>;

fn compaction<'a>(node: &'a mut Node, outcome: &'a mut Outcome) -> Run<'a> {
    Box::pin(compaction_script(node, outcome))
}

/// Recover from the images and check them.
async fn check(
    log_image: &SimImage,
    store_image: &engine::SimImage,
    outcome: &Outcome,
    hook: Hook,
) -> Result<(), String> {
    let fs = SimFs::from_image(log_image);
    let mut log = DiskLog::<Rt, SimFs>::open(fs, dir(), log_options(), flusher())
        .map_err(|error| format!("the log does not recover: {error}"))?;
    let mut sm = Sm::open_sim(
        SimStore::from_image(store_image),
        options(),
        hook(&log),
        Applier::Inline,
    )
    .map_err(|error| format!("the state machine does not recover: {error}"))?;
    let LogState {
        last_purged_log_id: purged,
        last_log_id: last,
    } = log.get_log_state().await.map_err(|e| e.to_string())?;
    let applied = sm.applied_state().await.map_err(|e| e.to_string())?.0;

    if let Some(durable) = outcome.durable {
        let held = last.map(|l| l.index).max(applied.map(|a| a.index));
        if held < Some(durable) {
            return Err(format!(
                "entry {durable} was durable, but the log ends at {last:?} and the state machine at {applied:?}"
            ));
        }
    }
    if purged.map(|p| p.index) > applied.map(|a| a.index) {
        return Err(format!(
            "the state machine ({applied:?}) is behind the purge point ({purged:?})"
        ));
    }
    if let (Some(applied), Some(last)) = (applied, last)
        && applied > last
        && applied.index <= last.index
    {
        return Err(format!(
            "inverted log order: applied {applied:?}, last {last:?}"
        ));
    }
    if let Some(installed) = outcome.installed
        && applied < Some(installed)
    {
        return Err(format!(
            "an installed snapshot ({installed:?}) was lost: applied {applied:?}"
        ));
    }

    // Contents equal the entries applied so far; a log id names its entry.
    let expect = |through: Option<OrLogId>| -> BTreeMap<Vec<u8>, Vec<u8>> {
        let mut map = BTreeMap::new();
        if let Some(through) = through {
            for index in 0..=through.index {
                let term = if through.leader_id.term == 2 && index > 5 {
                    2
                } else {
                    1
                };
                let e = entry(term, index);
                if let crate::types::OrPayload::Normal(command) = &e.payload
                    && let crate::command::Op::Put { key, value } = &command.ops[0]
                {
                    map.insert(key.clone(), value.clone());
                }
            }
        }
        map
    };
    if sm.user_contents() != expect(applied) {
        return Err(format!(
            "the contents do not match the applied log id {applied:?}"
        ));
    }

    // Redeliver the log above the state machine.
    let from = applied.map_or(0, |a| a.index + 1);
    let redelivered = log
        .try_get_log_entries(from..)
        .await
        .map_err(|e| e.to_string())?;
    let caught_up = redelivered.last().map(|e| e.log_id).or(applied);
    apply(&mut sm, redelivered)
        .await
        .map_err(|e| e.to_string())?;
    if sm.user_contents() != expect(caught_up) {
        return Err("redelivery does not reach the log's contents".to_owned());
    }

    // openraft accepts the recovered storage.
    let router: Arc<Router<Sm, Rt>> = Router::new();
    let node = RaftNode::<Sm, Rt>::start(1, &RaftConfig::default(), log, sm, router)
        .await
        .map_err(|error| format!("openraft refuses the storage: {error}"))?;
    node.shutdown().await;
    Ok(())
}

/// Run `script` with one disk cut at every operation, then crash both.
/// Returns the number of crashes checked, or the first failure.
fn crash_everywhere(script: Script, hook: Hook) -> Result<u64, String> {
    let (fs_total, store_total) = SimRuntime::with_seed(1).block_on(async {
        let mut node = Node::new(hook);
        script(&mut node, &mut Outcome::default()).await.unwrap();
        node.ops()
    });
    let mut checked = 0;
    for disk in [0, 1] {
        let total = if disk == 0 { fs_total } else { store_total };
        for cut in 0..=total {
            for seed in 0..4 {
                let result = SimRuntime::with_seed(cut * 7 + seed).block_on(async {
                    let mut node = Node::new(hook);
                    match disk {
                        0 => node.fs.cut_power_at(Some(node.fs.ops() + cut)),
                        _ => node.store().die_after(cut),
                    }
                    let mut outcome = Outcome::default();
                    let _ = script(&mut node, &mut outcome).await;
                    let (log, store) = node.crash(cut * 131 + seed);
                    check(&log, &store, &outcome, hook).await
                });
                result.map_err(|error| {
                    let disk = ["log", "store"][disk];
                    format!("{disk} cut at {cut}, seed {seed}: {error}")
                })?;
                checked += 1;
            }
        }
    }
    Ok(checked)
}

fn install<'a>(node: &'a mut Node, outcome: &'a mut Outcome) -> Run<'a> {
    Box::pin(install_script(node, outcome))
}

#[test]
fn compaction_crashed_at_every_operation_recovers() {
    let checked = crash_everywhere(compaction, real_hook).unwrap_or_else(|error| panic!("{error}"));
    assert!(checked > 100, "{checked} crashes");
}

#[test]
fn a_snapshot_install_crashed_at_every_operation_recovers() {
    let checked = crash_everywhere(install, real_hook).unwrap_or_else(|error| panic!("{error}"));
    assert!(checked > 30, "{checked} crashes");
}

#[test]
fn without_the_sync_hook_an_install_breaks_the_log_order() {
    let error = crash_everywhere(install, broken_hook).unwrap_err();
    assert!(
        error.contains("inverted log order") || error.contains("openraft refuses"),
        "{error}"
    );
}
