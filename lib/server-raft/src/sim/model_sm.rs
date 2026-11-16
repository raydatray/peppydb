//! `ModelSm`: a state machine that is durable only at snapshots (plan 0028,
//! "Storage models").
//!
//! It stands in for `engine::Db` in `Mode::External` (plan 0022), which
//! plan 0025, S6, puts behind openraft, and keeps the same contract:
//!
//! - **One keyspace holds data and Raft state.** The applied log id and the
//!   membership are system keys (`0x00 ‖ name`) in the same map as the user
//!   data, written with each entry, so they always describe the data
//!   (plan 0025, "The system keyspace").
//! - **Applied is not durable.** `apply` changes only the live map. A
//!   snapshot is a checkpoint: it copies the live map into the durable
//!   image, after the log hook makes every queued log write durable
//!   (plan 0025, decision 2: `checkpoint_with(|seq| raft_log.sync_to(seq))`).
//! - **A crash rolls back to the last snapshot,** as a reopen of the engine
//!   returns to its last committed checkpoint. openraft then re-applies
//!   from the log, starting after the applied log id that the system keys
//!   report.
//! - **Redelivered entries are skipped,** as `Db::apply` acknowledges an
//!   entry at or below its applied sequence without effect.
//! - **An installed snapshot is durable at once,** as `install_snapshot`
//!   renames the new store file into place.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use futures_util::Stream;
use futures_util::StreamExt;
use openraft::AsyncRuntime;
use openraft::EntryPayload;
use openraft::RaftSnapshotBuilder;
use openraft::storage::EntryResponder;
use openraft::storage::RaftStateMachine;

use crate::command::Op;
use crate::types::Membership;
use crate::types::OrLogId;
use crate::types::OrSnapshot;
use crate::types::OrSnapshotMeta;
use crate::types::OrStoredMembership;
use crate::types::PeerInfo;
use crate::types::Types;

/// `0x00 ‖ "raft/applied"`: the last applied log id, `term u64 | index u64`.
pub const APPLIED_KEY: &[u8] = b"\x00raft/applied";
/// `0x00 ‖ "raft/membership"`: the last applied membership and its log id.
pub const MEMBERSHIP_KEY: &[u8] = b"\x00raft/membership";

/// The contents of a model state machine: data and system keys together.
pub type Contents = BTreeMap<Vec<u8>, Vec<u8>>;

/// A durable snapshot: what survives a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSnapshot {
    pub meta: OrSnapshotMeta,
    pub data: Vec<u8>,
}

/// The log hook of a checkpoint: make every queued log write durable.
pub type SyncHook = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    live: Contents,
    durable: Option<StoredSnapshot>,
    crashed: bool,
    sync: SyncHook,
}

impl Inner {
    fn check(&self) -> Result<(), io::Error> {
        match self.crashed {
            true => Err(io::Error::other("the state machine crashed")),
            false => Ok(()),
        }
    }
}

/// A state machine that is durable only at snapshots. Clones share one
/// state machine.
pub struct ModelSm<RT> {
    inner: Arc<Mutex<Inner>>,
    _rt: PhantomData<fn() -> RT>,
}

impl<RT> Clone for ModelSm<RT> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _rt: PhantomData,
        }
    }
}

impl<RT> ModelSm<RT> {
    /// An empty state machine. `sync` makes the node's log durable; a
    /// snapshot calls it before it becomes durable.
    pub fn new(sync: SyncHook) -> Self {
        Self::from_durable(None, sync)
    }

    /// A state machine restarted from what a crashed one left durable.
    pub fn from_durable(durable: Option<StoredSnapshot>, sync: SyncHook) -> Self {
        let live = durable
            .as_ref()
            .map(|snapshot| decode_contents(&snapshot.data))
            .transpose()
            .unwrap_or_else(|error| panic!("a durable snapshot does not decode: {error}"))
            .unwrap_or_default();
        let inner = Inner {
            live,
            durable,
            crashed: false,
            sync,
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            _rt: PhantomData,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The live value of a stored key.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.lock().live.get(key).cloned()
    }

    /// The live contents, system keys included.
    pub fn contents(&self) -> Contents {
        self.lock().live.clone()
    }

    /// The live contents without system keys.
    pub fn user_contents(&self) -> Contents {
        let live = &self.lock().live;
        live.range(vec![0x01]..)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// The last applied log id, from the live system key.
    pub fn applied(&self) -> Option<OrLogId> {
        applied_of(&self.lock().live)
    }

    /// The durable snapshot.
    pub fn durable(&self) -> Option<StoredSnapshot> {
        self.lock().durable.clone()
    }

    /// Crash: drop everything since the last snapshot, freeze the state
    /// machine, and return what survives.
    pub fn crash(&self) -> Option<StoredSnapshot> {
        let mut inner = self.lock();
        inner.crashed = true;
        inner.durable.clone()
    }
}

impl<RT: AsyncRuntime> RaftSnapshotBuilder<Types<RT>> for ModelSm<RT> {
    type SnapshotData = Vec<u8>;

    async fn build_snapshot(&mut self) -> Result<OrSnapshot, io::Error> {
        let mut inner = self.lock();
        inner.check()?;
        let data = encode_contents(&inner.live);
        let meta = meta_of(&inner.live);
        // The checkpoint hook: the image may hold entries that only the log
        // can redeliver, so the log must be durable through them before the
        // image is (engine owner obligation 4).
        (inner.sync)();
        let snapshot = StoredSnapshot { meta, data };
        inner.durable = Some(snapshot.clone());
        Ok(OrSnapshot {
            meta: snapshot.meta,
            snapshot: snapshot.data,
        })
    }
}

impl<RT: AsyncRuntime> RaftStateMachine<Types<RT>> for ModelSm<RT> {
    type SnapshotData = Vec<u8>;
    type SnapshotBuilder = Self;

    async fn applied_state(&mut self) -> Result<(Option<OrLogId>, OrStoredMembership), io::Error> {
        let inner = self.lock();
        inner.check()?;
        Ok((applied_of(&inner.live), membership_of(&inner.live)))
    }

    async fn apply<S>(&mut self, mut entries: S) -> Result<(), io::Error>
    where
        S: Stream<Item = Result<EntryResponder<Types<RT>>, io::Error>> + Unpin + Send,
    {
        while let Some(item) = entries.next().await {
            let (entry, responder) = item?;
            {
                let mut inner = self.lock();
                inner.check()?;
                let live = &mut inner.live;
                if applied_of(live) < Some(entry.log_id) {
                    match &entry.payload {
                        EntryPayload::Blank => {}
                        EntryPayload::Normal(command) => {
                            for op in &command.ops {
                                match op {
                                    Op::Put { key, value } => {
                                        live.insert(key.clone(), value.clone())
                                    }
                                    Op::Delete { key } => live.remove(key),
                                };
                            }
                        }
                        EntryPayload::Membership(membership) => {
                            let stored =
                                OrStoredMembership::new(Some(entry.log_id), membership.clone());
                            live.insert(MEMBERSHIP_KEY.to_vec(), encode_membership(&stored));
                        }
                    }
                    live.insert(APPLIED_KEY.to_vec(), encode_log_id(&entry.log_id));
                }
            }
            if let Some(responder) = responder {
                responder.send(());
            }
        }
        Ok(())
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn install_snapshot(
        &mut self,
        meta: &OrSnapshotMeta,
        snapshot: Vec<u8>,
    ) -> Result<(), io::Error> {
        let mut inner = self.lock();
        inner.check()?;
        let live = decode_contents(&snapshot)?;
        if meta_of(&live) != *meta {
            return Err(io::Error::other(
                "a snapshot's metadata does not match its system keys",
            ));
        }
        inner.live = live;
        inner.durable = Some(StoredSnapshot {
            meta: meta.clone(),
            data: snapshot,
        });
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<OrSnapshot>, io::Error> {
        let inner = self.lock();
        inner.check()?;
        Ok(inner.durable.clone().map(|snapshot| OrSnapshot {
            meta: snapshot.meta,
            snapshot: snapshot.data,
        }))
    }
}

fn applied_of(contents: &Contents) -> Option<OrLogId> {
    contents.get(APPLIED_KEY).map(|bytes| {
        decode_log_id(&mut Reader(bytes)).unwrap_or_else(|error| panic!("bad applied key: {error}"))
    })
}

fn membership_of(contents: &Contents) -> OrStoredMembership {
    contents
        .get(MEMBERSHIP_KEY)
        .map(|bytes| {
            decode_membership(bytes).unwrap_or_else(|error| panic!("bad membership key: {error}"))
        })
        .unwrap_or_default()
}

fn meta_of(contents: &Contents) -> OrSnapshotMeta {
    OrSnapshotMeta {
        last_log_id: applied_of(contents),
        last_membership: membership_of(contents),
    }
}

// The encodings of the system keys and of a snapshot. Integers are
// little-endian; lengths are u32.

fn encode_log_id(log_id: &OrLogId) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&log_id.leader_id.term.to_le_bytes());
    out.extend_from_slice(&log_id.index.to_le_bytes());
    out
}

fn encode_membership(stored: &OrStoredMembership) -> Vec<u8> {
    let mut out = Vec::new();
    match stored.log_id() {
        Some(log_id) => {
            out.push(1);
            out.extend(encode_log_id(log_id));
        }
        None => out.push(0),
    }
    let configs = stored.get_joint_config();
    put_u32(&mut out, configs.len());
    for config in configs {
        put_u32(&mut out, config.len());
        for id in config {
            out.extend_from_slice(&id.to_le_bytes());
        }
    }
    let nodes: Vec<_> = stored.nodes().collect();
    put_u32(&mut out, nodes.len());
    for (id, node) in nodes {
        out.extend_from_slice(&id.to_le_bytes());
        put_bytes(&mut out, node.addr.as_bytes());
    }
    out
}

fn decode_membership(bytes: &[u8]) -> Result<OrStoredMembership, io::Error> {
    let mut reader = Reader(bytes);
    let log_id = match reader.u8()? {
        0 => None,
        _ => Some(decode_log_id(&mut reader)?),
    };
    let configs = (0..reader.u32()?)
        .map(|_| (0..reader.u32()?).map(|_| reader.u64()).collect())
        .collect::<Result<Vec<BTreeSet<u64>>, io::Error>>()?;
    let nodes = (0..reader.u32()?)
        .map(|_| {
            let id = reader.u64()?;
            let addr = String::from_utf8(reader.bytes()?).map_err(io::Error::other)?;
            Ok((id, PeerInfo { addr }))
        })
        .collect::<Result<BTreeMap<u64, PeerInfo>, io::Error>>()?;
    let membership = Membership::new(configs, nodes).map_err(io::Error::other)?;
    Ok(OrStoredMembership::new(log_id, membership))
}

fn encode_contents(contents: &Contents) -> Vec<u8> {
    let mut out = Vec::new();
    put_u32(&mut out, contents.len());
    for (key, value) in contents {
        put_bytes(&mut out, key);
        put_bytes(&mut out, value);
    }
    out
}

fn decode_contents(bytes: &[u8]) -> Result<Contents, io::Error> {
    let mut reader = Reader(bytes);
    let contents = (0..reader.u32()?)
        .map(|_| Ok((reader.bytes()?, reader.bytes()?)))
        .collect::<Result<_, io::Error>>()?;
    match reader.0.len() {
        0 => Ok(contents),
        _ => Err(io::Error::other("trailing bytes after a snapshot")),
    }
}

fn put_u32(out: &mut Vec<u8>, n: usize) {
    let n = u32::try_from(n).unwrap_or_else(|_| panic!("{n} does not fit in u32"));
    out.extend_from_slice(&n.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len());
    out.extend_from_slice(bytes);
}

fn decode_log_id(reader: &mut Reader<'_>) -> Result<OrLogId, io::Error> {
    let term = reader.u64()?;
    let index = reader.u64()?;
    Ok(OrLogId::new_term_index(term, index))
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], io::Error> {
        if self.0.len() < n {
            return Err(io::Error::other("a system value ends early"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, io::Error> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, io::Error> {
        let bytes: [u8; 4] = self.take(4)?.try_into().map_err(io::Error::other)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, io::Error> {
        let bytes: [u8; 8] = self.take(8)?.try_into().map_err(io::Error::other)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, io::Error> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use futures_util::stream;
    use openraft::StorageError;
    use openraft::entry::RaftEntry;
    use openraft::testing::log::StoreBuilder;
    use openraft::testing::log::Suite;
    use openraft_rt_sim::SimRuntime;

    use super::*;
    use crate::command::Command;
    use crate::sim::LogConfig;
    use crate::sim::MemLog;
    use crate::types::OrEntry;
    use crate::types::OrPayload;
    use crate::types::log_id;

    type Sm = ModelSm<SimRuntime>;

    fn no_sync() -> SyncHook {
        Arc::new(|| {})
    }

    fn put(term: u64, index: u64, key: &[u8], value: &[u8]) -> OrEntry {
        let command = Command {
            ops: vec![Op::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            }],
        };
        OrEntry::new(log_id(term, index), OrPayload::Normal(command))
    }

    fn members(ids: &[u64]) -> Membership {
        let nodes: BTreeMap<u64, PeerInfo> =
            ids.iter().map(|id| (*id, PeerInfo::default())).collect();
        Membership::from(nodes)
    }

    async fn apply(sm: &mut Sm, entries: Vec<OrEntry>) {
        let items = stream::iter(entries.into_iter().map(|entry| Ok((entry, None))));
        sm.apply(items).await.unwrap();
    }

    struct Builder;

    impl StoreBuilder<Types<SimRuntime>, MemLog<SimRuntime>, Sm> for Builder {
        async fn build(
            &self,
        ) -> Result<((), MemLog<SimRuntime>, Sm), StorageError<Types<SimRuntime>>> {
            let log = MemLog::new(LogConfig::default());
            let hook_log = log.clone();
            let sm = ModelSm::new(Arc::new(move || hook_log.sync()));
            Ok(((), log, sm))
        }
    }

    /// openraft's storage conformance suite, run against both models.
    #[test]
    fn the_models_pass_openraft_s_storage_suite() {
        SimRuntime::with_seed(1).block_on(async {
            Suite::test_all(Builder).await.unwrap();
        });
    }

    #[test]
    fn applied_state_lives_in_system_keys_beside_the_data() {
        SimRuntime::with_seed(2).block_on(async {
            let mut sm = Sm::new(no_sync());
            let membership = OrEntry::new(log_id(1, 1), OrPayload::Membership(members(&[1, 2, 3])));
            apply(&mut sm, vec![membership, put(1, 2, b"\x01a", b"1")]).await;

            let contents = sm.contents();
            assert_eq!(contents.get(&b"\x01a"[..]), Some(&b"1".to_vec()));
            assert!(contents.contains_key(APPLIED_KEY) && contents.contains_key(MEMBERSHIP_KEY));
            let (applied, stored) = sm.applied_state().await.unwrap();
            assert_eq!(applied, Some(log_id(1, 2)));
            assert_eq!(stored.log_id(), &Some(log_id(1, 1)));
            assert_eq!(stored.voter_ids().collect::<Vec<_>>(), [1, 2, 3]);
            assert_eq!(
                sm.user_contents().len(),
                1,
                "user contents hide system keys"
            );
        });
    }

    #[test]
    fn a_crash_rolls_back_to_the_last_snapshot() {
        SimRuntime::with_seed(3).block_on(async {
            let mut sm = Sm::new(no_sync());
            apply(&mut sm, vec![put(1, 1, b"\x01a", b"1")]).await;
            let snapshot = sm
                .get_snapshot_builder()
                .await
                .build_snapshot()
                .await
                .unwrap();
            assert_eq!(snapshot.meta.last_log_id, Some(log_id(1, 1)));
            apply(&mut sm, vec![put(1, 2, b"\x01a", b"2")]).await;

            let durable = sm.crash();
            assert!(
                sm.applied_state().await.is_err(),
                "a crashed state machine refuses calls"
            );
            let mut restarted = Sm::from_durable(durable, no_sync());
            assert_eq!(restarted.get(b"\x01a"), Some(b"1".to_vec()));
            assert_eq!(
                restarted.applied_state().await.unwrap().0,
                Some(log_id(1, 1))
            );

            // openraft redelivers from the log: everything at or below the
            // applied log id is skipped, the rest applies, and the state
            // converges.
            apply(
                &mut restarted,
                vec![put(1, 1, b"\x01a", b"stale"), put(1, 2, b"\x01a", b"2")],
            )
            .await;
            assert_eq!(restarted.get(b"\x01a"), Some(b"2".to_vec()));
            assert_eq!(restarted.applied(), Some(log_id(1, 2)));
        });
    }

    #[test]
    fn a_snapshot_makes_the_log_durable_before_itself() {
        SimRuntime::with_seed(4).block_on(async {
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&calls);
            let mut sm = Sm::new(Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }));
            apply(&mut sm, vec![put(1, 1, b"\x01a", b"1")]).await;
            assert_eq!(calls.load(Ordering::SeqCst), 0, "applying does not sync");
            sm.get_snapshot_builder()
                .await
                .build_snapshot()
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn an_install_replaces_state_and_metadata_together() {
        SimRuntime::with_seed(5).block_on(async {
            let mut leader = Sm::new(no_sync());
            let membership = OrEntry::new(log_id(1, 1), OrPayload::Membership(members(&[1, 2])));
            apply(&mut leader, vec![membership, put(2, 5, b"\x01k", b"v")]).await;
            let snapshot = leader
                .get_snapshot_builder()
                .await
                .build_snapshot()
                .await
                .unwrap();

            let mut follower = Sm::new(no_sync());
            apply(&mut follower, vec![put(1, 1, b"\x01old", b"x")]).await;
            follower
                .install_snapshot(&snapshot.meta, snapshot.snapshot.clone())
                .await
                .unwrap();
            assert_eq!(follower.contents(), leader.contents());
            assert_eq!(
                follower.applied_state().await.unwrap().0,
                Some(log_id(2, 5))
            );
            let durable = follower.crash();
            assert_eq!(
                durable.map(|s| s.meta),
                Some(snapshot.meta.clone()),
                "durable at once"
            );

            let mut other = Sm::new(no_sync());
            let mut wrong = snapshot.meta.clone();
            wrong.last_log_id = Some(log_id(2, 6));
            assert!(
                other
                    .install_snapshot(&wrong, snapshot.snapshot)
                    .await
                    .is_err()
            );
        });
    }
}
