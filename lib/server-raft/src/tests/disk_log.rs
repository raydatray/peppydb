//! `DiskLog` (plan 0029, diff 7): openraft's storage suite over both file
//! systems, durability of votes and appends, and fail-stop.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use openraft::AsyncRuntime;
use openraft::RaftLogReader;
use openraft::StorageError;
use openraft::entry::RaftEntry;
use openraft::storage::IOFlushed;
use openraft::storage::RaftLogStorage;
use openraft::testing::log::StoreBuilder;
use openraft::testing::log::Suite;
use openraft::type_config::TypeConfigExt;
use openraft_rt_sim::SimRuntime;
use openraft_rt_tokio::TokioRuntime;
use server_raft_log::Fs;
use server_raft_log::LogOptions;
use server_raft_log::RealFs;
use server_raft_log::SimFs;

use crate::command::Command;
use crate::command::Op;
use crate::disk_log::DiskLog;
use crate::disk_log::Flusher;
use crate::disk_log::to_log_entry;
use crate::disk_log::to_or_entry;
use crate::error::ProposeError;
use crate::node::RaftConfig;
use crate::node::RaftNode;
use crate::node::Role;
use crate::sim::ModelSm;
use crate::sim::Router;
use crate::types::Membership;
use crate::types::OrEntry;
use crate::types::OrPayload;
use crate::types::OrVote;
use crate::types::PeerInfo;
use crate::types::Types;
use crate::types::log_id;

fn dir() -> &'static Path {
    Path::new("/raft-log")
}

fn small() -> LogOptions {
    LogOptions {
        segment_bytes: 512,
        cache_bytes: 1024,
    }
}

fn task(max_delay: Duration) -> Flusher {
    Flusher::Task { max_delay, seed: 7 }
}

/// A log and a model state machine whose checkpoint hook syncs it.
fn storage<RT: AsyncRuntime, F: Fs>(log: &DiskLog<RT, F>) -> ModelSm<RT> {
    let hook = log.clone();
    ModelSm::new(Arc::new(move || {
        hook.sync().expect("the log syncs");
    }))
}

struct SimBuilder(Duration);

impl StoreBuilder<Types<SimRuntime>, DiskLog<SimRuntime, SimFs>, ModelSm<SimRuntime>>
    for SimBuilder
{
    async fn build(
        &self,
    ) -> Result<
        ((), DiskLog<SimRuntime, SimFs>, ModelSm<SimRuntime>),
        StorageError<Types<SimRuntime>>,
    > {
        let log = DiskLog::open(SimFs::new(), dir(), small(), task(self.0)).unwrap();
        let sm = storage(&log);
        Ok(((), log, sm))
    }
}

struct RealBuilder;

impl
    StoreBuilder<
        Types<TokioRuntime>,
        DiskLog<TokioRuntime>,
        ModelSm<TokioRuntime>,
        tempfile::TempDir,
    > for RealBuilder
{
    async fn build(
        &self,
    ) -> Result<
        (
            tempfile::TempDir,
            DiskLog<TokioRuntime>,
            ModelSm<TokioRuntime>,
        ),
        StorageError<Types<TokioRuntime>>,
    > {
        let tmp = tempfile::tempdir().unwrap();
        let log = DiskLog::open(RealFs, tmp.path(), small(), Flusher::Thread).unwrap();
        let sm = storage(&log);
        Ok((tmp, log, sm))
    }
}

#[test]
fn the_adapter_passes_openraft_s_storage_suite_on_simulated_files() {
    for delay in [Duration::ZERO, Duration::from_millis(3)] {
        SimRuntime::with_seed(1).block_on(async {
            Suite::test_all(SimBuilder(delay)).await.unwrap();
        });
    }
}

#[test]
fn the_adapter_passes_openraft_s_storage_suite_on_real_files() {
    TokioRuntime::run(async {
        Suite::test_all(RealBuilder).await.unwrap();
    });
}

fn members(ids: &[u64]) -> Membership {
    let nodes: BTreeMap<u64, PeerInfo> = ids.iter().map(|id| (*id, PeerInfo::default())).collect();
    Membership::from(nodes)
}

#[test]
fn entries_convert_both_ways() {
    let command = Command {
        ops: vec![
            Op::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            },
            Op::Delete { key: b"d".to_vec() },
        ],
    };
    for payload in [
        OrPayload::Blank,
        OrPayload::Normal(command),
        OrPayload::Membership(members(&[1, 2, 3])),
    ] {
        let entry = OrEntry::new(log_id(4, 9), payload);
        let stored = to_log_entry(&entry).unwrap();
        assert_eq!((stored.term, stored.index), (4, 9));
        assert_eq!(to_or_entry(stored).unwrap(), entry);
    }
}

#[test]
fn votes_and_appends_survive_a_reopen_on_real_files() {
    let tmp = tempfile::tempdir().unwrap();
    TokioRuntime::run(async {
        let mut log =
            DiskLog::<TokioRuntime>::open(RealFs, tmp.path(), small(), Flusher::Thread).unwrap();
        let entries: Vec<_> = (1..=40)
            .map(|i| OrEntry::new(log_id(1, i), OrPayload::Blank))
            .collect();
        let (tx, rx) = Types::<TokioRuntime>::oneshot();
        log.append(entries, IOFlushed::signal(tx)).await.unwrap();
        rx.await.unwrap().unwrap();
        log.save_vote(&OrVote::new_committed(3, 2)).await.unwrap();
        drop(log);

        let mut log =
            DiskLog::<TokioRuntime>::open(RealFs, tmp.path(), small(), Flusher::Thread).unwrap();
        assert_eq!(
            log.read_vote().await.unwrap(),
            Some(OrVote::new_committed(3, 2))
        );
        let state = log.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id, Some(log_id(1, 40)));
        // Below the tiny cache: read from the segments on the blocking pool.
        let read = log.try_get_log_entries(2..5).await.unwrap();
        assert_eq!(
            read.iter().map(|e| e.log_id.index).collect::<Vec<_>>(),
            [2, 3, 4]
        );
    });
}

#[test]
fn a_failed_sync_fails_the_waiting_append_and_every_later_write() {
    SimRuntime::with_seed(3).block_on(async {
        let fs = SimFs::new();
        let mut log = DiskLog::<SimRuntime, SimFs>::open(
            fs.clone(),
            dir(),
            small(),
            task(Duration::from_millis(1)),
        )
        .unwrap();
        let blank = |i| OrEntry::new(log_id(1, i), OrPayload::Blank);
        let (tx, rx) = Types::<SimRuntime>::oneshot();
        log.append([blank(1)], IOFlushed::signal(tx)).await.unwrap();
        rx.await.unwrap().unwrap();

        // The next group's sync fails: write, then sync.
        fs.fail_op(Some(fs.ops() + 1));
        let (tx, rx) = Types::<SimRuntime>::oneshot();
        log.append([blank(2)], IOFlushed::signal(tx)).await.unwrap();
        assert!(
            rx.await.unwrap().is_err(),
            "the callback reports the failure"
        );
        assert!(log.append([blank(3)], IOFlushed::noop()).await.is_err());
        assert!(log.save_vote(&OrVote::new(2, 1)).await.is_err());
        assert!(log.sync().is_err());
    });
}

#[test]
fn a_failed_sync_stops_the_node() {
    SimRuntime::with_seed(4).block_on(async {
        let fs = SimFs::new();
        let log = DiskLog::<SimRuntime, SimFs>::open(
            fs.clone(),
            dir(),
            small(),
            task(Duration::from_millis(1)),
        )
        .unwrap();
        let sm = storage(&log);
        let router = Router::new();
        let node = RaftNode::<ModelSm<SimRuntime>, SimRuntime>::start(
            1,
            &RaftConfig::default(),
            log,
            sm,
            Arc::clone(&router),
        )
        .await
        .unwrap();
        router.register(node.clone());
        node.initialize([(1, PeerInfo::default())].into())
            .await
            .unwrap();
        for _ in 0..500 {
            if node.status().role == Role::Leader {
                break;
            }
            SimRuntime::sleep(Duration::from_millis(10)).await;
        }
        let put = |v: u8| Command {
            ops: vec![Op::Put {
                key: b"\x01k".to_vec(),
                value: vec![v],
            }],
        };
        node.propose(put(1)).await.unwrap();

        // Every later sync fails.
        fs.cut_power_at(Some(fs.ops()));
        let error = node.propose(put(2)).await.unwrap_err();
        assert!(
            matches!(
                error,
                ProposeError::Stopped(_) | ProposeError::OutcomeUnknown
            ),
            "{error:?}"
        );
        assert!(matches!(
            node.propose(put(3)).await,
            Err(ProposeError::Stopped(_))
        ));
        node.shutdown().await;
    });
}
