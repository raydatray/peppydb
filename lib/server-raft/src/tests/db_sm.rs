//! `DbStateMachine` (plan 0030): applies, the system keys, redelivery, and
//! the applier.

use std::collections::BTreeMap;
use std::sync::Arc;

use engine::Db;
use engine::Mode;
use engine::Options;
use engine::SimStore;
use engine::TreeConfig;
use futures_util::stream;
use openraft::AsyncRuntime;
use openraft::entry::RaftEntry;
use openraft::storage::RaftStateMachine;
use openraft_rt_sim::SimRuntime;
use openraft_rt_tokio::TokioRuntime;

use crate::command::Command;
use crate::command::Op;
use crate::db_sm::Applier;
use crate::db_sm::DbStateMachine;
use crate::db_sm::SyncHook;
use crate::types::Membership;
use crate::types::OrEntry;
use crate::types::OrPayload;
use crate::types::PeerInfo;
use crate::types::log_id;

/// External mode with limits that hold a membership value.
pub(super) fn options() -> Options {
    Options {
        mode: Mode::External,
        tree: TreeConfig {
            circular_buffer_bytes: 1 << 20,
            ..TreeConfig::default()
        },
        ..Options::default()
    }
}

pub(super) fn sim_db() -> (Db, SimStore) {
    let store = Db::new_sim_store(&options()).unwrap();
    (
        Db::open_external_on(store.clone(), options()).unwrap(),
        store,
    )
}

pub(super) fn no_sync() -> SyncHook {
    Arc::new(|| Ok(()))
}

pub(super) fn put(term: u64, index: u64, key: &[u8], value: &[u8]) -> OrEntry {
    let command = Command {
        ops: vec![Op::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        }],
    };
    OrEntry::new(log_id(term, index), OrPayload::Normal(command))
}

pub(super) fn members(ids: &[u64]) -> Membership {
    let nodes: BTreeMap<u64, PeerInfo> = ids.iter().map(|id| (*id, PeerInfo::default())).collect();
    Membership::from(nodes)
}

async fn apply<RT: AsyncRuntime>(sm: &mut DbStateMachine<RT>, entries: Vec<OrEntry>) {
    let items = stream::iter(entries.into_iter().map(|entry| Ok((entry, None))));
    sm.apply(items).await.unwrap();
}

#[test]
fn entries_apply_with_the_applied_log_id_and_membership() {
    SimRuntime::with_seed(1).block_on(async {
        let (db, _) = sim_db();
        let mut sm = DbStateMachine::<SimRuntime>::open(db, no_sync(), Applier::Inline).unwrap();
        assert_eq!(sm.applied_state().await.unwrap().0, None);

        let membership = OrEntry::new(log_id(1, 1), OrPayload::Membership(members(&[1, 2, 3])));
        let blank = OrEntry::new(log_id(1, 2), OrPayload::Blank);
        apply(&mut sm, vec![membership, blank, put(1, 3, b"\x01a", b"1")]).await;

        let (applied, stored) = sm.applied_state().await.unwrap();
        assert_eq!(applied, Some(log_id(1, 3)));
        assert_eq!(stored.log_id(), &Some(log_id(1, 1)));
        assert_eq!(stored.membership(), &members(&[1, 2, 3]));
        assert_eq!(sm.get(b"\x01a"), Some(b"1".to_vec()));
        assert_eq!(
            sm.user_contents(),
            BTreeMap::from([(b"\x01a".to_vec(), b"1".to_vec())]),
            "system keys are not user contents"
        );

        // A blank entry advances the applied log id.
        apply(&mut sm, vec![OrEntry::new(log_id(2, 4), OrPayload::Blank)]).await;
        assert_eq!(sm.applied_state().await.unwrap().0, Some(log_id(2, 4)));
    });
}

#[test]
fn redelivered_entries_are_skipped() {
    SimRuntime::with_seed(2).block_on(async {
        let (db, _) = sim_db();
        let mut sm = DbStateMachine::<SimRuntime>::open(db, no_sync(), Applier::Inline).unwrap();
        apply(&mut sm, vec![put(1, 1, b"\x01k", b"new")]).await;
        apply(&mut sm, vec![put(1, 1, b"\x01k", b"old")]).await;
        assert_eq!(sm.get(b"\x01k"), Some(b"new".to_vec()));
    });
}

#[test]
fn a_crash_rolls_back_to_the_committed_checkpoint_with_the_system_keys() {
    SimRuntime::with_seed(3).block_on(async {
        let (db, store) = sim_db();
        // Commit through index 2, then apply 3 without a checkpoint.
        let mut sm = DbStateMachine::<SimRuntime>::open(db, no_sync(), Applier::Inline).unwrap();
        apply(
            &mut sm,
            vec![put(1, 1, b"\x01a", b"1"), put(1, 2, b"\x01b", b"2")],
        )
        .await;
        sm.db().unwrap().checkpoint_with(|_| Ok(())).unwrap();
        apply(&mut sm, vec![put(1, 3, b"\x01c", b"3")]).await;
        drop(sm);

        let image = store.crash(1);
        let db = Db::open_external_on(SimStore::from_image(&image), options()).unwrap();
        let mut sm = DbStateMachine::<SimRuntime>::open(db, no_sync(), Applier::Inline).unwrap();
        assert_eq!(sm.applied_state().await.unwrap().0, Some(log_id(1, 2)));
        assert_eq!(sm.get(b"\x01c"), None);
        // openraft redelivers from there.
        apply(&mut sm, vec![put(1, 3, b"\x01c", b"3")]).await;
        assert_eq!(sm.get(b"\x01c"), Some(b"3".to_vec()));
    });
}

#[test]
fn an_entry_that_cannot_apply_stops_the_state_machine() {
    SimRuntime::with_seed(4).block_on(async {
        let (db, _) = sim_db();
        let mut sm = DbStateMachine::<SimRuntime>::open(db, no_sync(), Applier::Inline).unwrap();
        let too_long = vec![1; 4096];
        let items = stream::iter([Ok((put(1, 1, &too_long, b"v"), None))]);
        assert!(sm.apply(items).await.is_err());
        assert!(sm.applied_state().await.is_err(), "every later call fails");
    });
}

#[test]
fn the_applier_thread_applies_in_order_and_answers_each_entry() {
    let dir = tempfile::tempdir().unwrap();
    TokioRuntime::run(async {
        let db = Db::open(dir.path(), options()).unwrap();
        let mut sm = DbStateMachine::<TokioRuntime>::open(db, no_sync(), Applier::Thread).unwrap();
        let entries: Vec<_> = (1..=200)
            .map(|i: u64| put(1, i, b"\x01k", &i.to_le_bytes()))
            .collect();
        apply(&mut sm, entries).await;
        assert_eq!(sm.applied_state().await.unwrap().0, Some(log_id(1, 200)));
        assert_eq!(sm.get(b"\x01k"), Some(200u64.to_le_bytes().to_vec()));
    });
}
