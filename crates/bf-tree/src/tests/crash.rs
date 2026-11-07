//! A fault-injecting store and a crash model against an oracle
//! (plan 0018).
//!
//! `FaultStore` keeps a durable image and the writes since the last sync.
//! `sync` and `commit` make writes durable. A crash keeps the durable image
//! and loses, lands, or tears each later write at 64-byte sectors. The store
//! can also fail a commit before or after its superblock copy lands, and it
//! can fail every call after the `k`-th, like a process that dies in the
//! middle of an operation (a pass, a split, an erase on open).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::*;

const SECTOR: usize = 64;

/// How the next commit fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitFault {
    /// The superblock copy never lands; the old state stays durable.
    BeforeCopy,
    /// The copy lands, but the call reports an error.
    AfterCopy,
}

#[derive(Debug)]
struct Disk {
    /// The pages as the process sees them.
    pages: Vec<Vec<u8>>,
    freed: Vec<bool>,
    free: Vec<usize>,
    state: State,
    /// What survives a crash.
    durable: Vec<Vec<u8>>,
    durable_state: State,
    /// Writes since the last sync, in order.
    pending: Vec<(usize, Vec<u8>)>,
    /// Calls left before every call fails; `None` for no limit.
    countdown: Option<u64>,
    dead: bool,
    commit_fault: Option<CommitFault>,
}

/// A handle to one simulated disk; clones share it.
#[derive(Clone, Debug)]
struct FaultStore {
    disk: Arc<Mutex<Disk>>,
    superblock: Superblock,
}

fn dead() -> StoreError {
    StoreError::Io {
        op: "fault",
        kind: std::io::ErrorKind::Other,
        code: Some(5),
    }
}

impl FaultStore {
    fn new(config: &TreeConfig) -> Self {
        Self::from_durable(Superblock::from(config), Vec::new(), State::default())
    }

    fn from_durable(superblock: Superblock, durable: Vec<Vec<u8>>, state: State) -> Self {
        let disk = Disk {
            pages: durable.clone(),
            freed: vec![false; durable.len()],
            free: Vec::new(),
            state,
            durable,
            durable_state: state,
            pending: Vec::new(),
            countdown: None,
            dead: false,
            commit_fault: None,
        };
        Self {
            disk: Arc::new(Mutex::new(disk)),
            superblock,
        }
    }

    /// Fail every store call after `calls` more.
    fn die_after(&self, calls: u64) {
        self.disk.lock().unwrap().countdown = Some(calls);
    }

    fn fail_next_commit(&self, fault: CommitFault) {
        self.disk.lock().unwrap().commit_fault = Some(fault);
    }

    fn durable_state(&self) -> State {
        self.disk.lock().unwrap().durable_state
    }

    /// The disk after a crash: the durable image, with each write since
    /// the last sync lost, landed, or torn at 64-byte sectors.
    fn crash(&self, rng: &mut Rng) -> FaultStore {
        let disk = self.disk.lock().unwrap();
        let mut durable = disk.durable.clone();
        for (index, bytes) in &disk.pending {
            match rng.below(3) {
                0 => {}
                1 => durable[*index].clone_from(bytes),
                _ => {
                    for sector in 0..bytes.len() / SECTOR {
                        if rng.below(2) == 0 {
                            let range = sector * SECTOR..(sector + 1) * SECTOR;
                            durable[*index][range.clone()].copy_from_slice(&bytes[range]);
                        }
                    }
                }
            }
        }
        FaultStore::from_durable(self.superblock, durable, disk.durable_state)
    }
}

impl Disk {
    /// Count one call; `Err` once the process has died.
    fn call(&mut self) -> Result<(), StoreError> {
        if let Some(left) = self.countdown.as_mut() {
            if *left == 0 {
                self.dead = true;
            } else {
                *left -= 1;
            }
        }
        if self.dead { Err(dead()) } else { Ok(()) }
    }

    fn sync(&mut self) {
        for (index, bytes) in self.pending.drain(..) {
            self.durable[index] = bytes;
        }
    }

    fn index(&self, page_number: PageNumber) -> Result<usize, StoreError> {
        let index = page_number.raw() as usize;
        if index < self.pages.len() {
            Ok(index)
        } else {
            Err(StoreError::UnknownPage(page_number))
        }
    }
}

impl PageStore for FaultStore {
    fn page_size(&self) -> usize {
        self.superblock.page_size
    }
    fn page_count(&self) -> u64 {
        self.disk.lock().unwrap().pages.len() as u64
    }
    fn alloc(&self) -> Result<PageNumber, StoreError> {
        let mut disk = self.disk.lock().unwrap();
        disk.call()?;
        let index = match disk.free.pop() {
            Some(index) => {
                disk.freed[index] = false;
                index
            }
            None => {
                // The file grows with a zero page.
                let zero = vec![0; self.superblock.page_size];
                disk.pages.push(zero.clone());
                disk.durable.push(zero);
                disk.freed.push(false);
                disk.pages.len() - 1
            }
        };
        Ok(PageNumber::new(index as u64).unwrap())
    }
    fn free(&self, page_number: PageNumber) -> Result<(), StoreError> {
        let mut disk = self.disk.lock().unwrap();
        disk.call()?;
        let index = disk.index(page_number)?;
        if disk.freed[index] {
            return Err(StoreError::FreedPage(page_number));
        }
        disk.freed[index] = true;
        disk.free.push(index);
        Ok(())
    }
    fn read(&self, page_number: PageNumber, buf: &mut [u8]) -> Result<(), StoreError> {
        let mut disk = self.disk.lock().unwrap();
        disk.call()?;
        let index = disk.index(page_number)?;
        buf.copy_from_slice(&disk.pages[index]);
        Ok(())
    }
    fn write(&self, page_number: PageNumber, buf: &[u8]) -> Result<(), StoreError> {
        let mut disk = self.disk.lock().unwrap();
        disk.call()?;
        let index = disk.index(page_number)?;
        if disk.freed[index] {
            return Err(StoreError::FreedPage(page_number));
        }
        disk.pages[index].copy_from_slice(buf);
        disk.pending.push((index, buf.to_vec()));
        Ok(())
    }
    fn sync(&self) -> Result<(), StoreError> {
        let mut disk = self.disk.lock().unwrap();
        disk.call()?;
        disk.sync();
        Ok(())
    }
    fn superblock(&self) -> &Superblock {
        &self.superblock
    }
    fn state(&self) -> State {
        self.disk.lock().unwrap().state
    }
    fn commit(&self, state: &State) -> Result<(), StoreError> {
        let mut disk = self.disk.lock().unwrap();
        disk.call()?;
        if disk.state.checkpoint_id + 1 != state.checkpoint_id {
            return Err(StoreError::CommitOrder {
                current: disk.state.checkpoint_id,
                got: state.checkpoint_id,
            });
        }
        // A commit writes a superblock copy and syncs the file, which also
        // makes every earlier page write durable.
        match disk.commit_fault.take() {
            Some(CommitFault::BeforeCopy) => Err(dead()),
            Some(CommitFault::AfterCopy) => {
                disk.sync();
                disk.durable_state = *state;
                Err(dead())
            }
            None => {
                disk.sync();
                disk.durable_state = *state;
                disk.state = *state;
                Ok(())
            }
        }
    }
}

/// A deterministic generator.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

#[derive(Clone, Debug)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Remove(Vec<u8>),
}

/// Every applied operation with its sequence, in sequence order.
#[derive(Default)]
struct Log(Vec<(u64, Op)>);

impl Log {
    /// The map that the operations at or below `seq` produce.
    fn state_at(&self, seq: u64) -> BTreeMap<Vec<u8>, Vec<u8>> {
        let mut map = BTreeMap::new();
        for (_, op) in self.0.iter().take_while(|(s, _)| *s <= seq) {
            match op {
                Op::Put(key, value) => {
                    map.insert(key.clone(), value.clone());
                }
                Op::Remove(key) => {
                    map.remove(key);
                }
            }
        }
        map
    }
}

/// Apply `op` to the tree; on success, log it with its sequence. A store
/// failure may reject it; it is then not applied.
fn apply(tree: &Tree, log: &mut Log, op: Op) -> bool {
    let result = match &op {
        Op::Put(key, value) => tree.put(key, value),
        Op::Remove(key) => tree.remove(key),
    };
    match result {
        Ok(()) => {
            log.0.push((tree.applied_seq(), op));
            true
        }
        Err(_) => false,
    }
}

fn random_op(rng: &mut Rng, config: &TreeConfig) -> Op {
    let key = (rng.below(400) as u32).to_be_bytes().to_vec();
    if rng.below(5) == 0 {
        Op::Remove(key)
    } else {
        let len = rng.below(config.max_value_len as u64 + 1) as usize;
        Op::Put(key, vec![rng.below(256) as u8; len])
    }
}

fn contents(tree: &Tree) -> BTreeMap<Vec<u8>, Vec<u8>> {
    tree.iter().map(Result::unwrap).collect()
}

/// Open the crashed disk, possibly dying during the open itself (for
/// example while it erases late pages); a death means another crash.
fn recover(store: FaultStore, config: TreeConfig, rng: &mut Rng) -> (Tree, FaultStore) {
    let mut store = store;
    loop {
        if rng.below(4) == 0 {
            store.die_after(rng.below(40));
        }
        match Tree::open_with(config, Box::new(store.clone())) {
            Ok(tree) => {
                store.disk.lock().unwrap().countdown = None;
                return (tree, store);
            }
            Err(TreeError::Store(_)) => store = store.crash(rng),
            Err(error) => panic!("open after a crash failed: {error:?}"),
        }
    }
}

/// One crash model run: `rounds` generations of writes, checkpoints, and a
/// crash, each recovered from the crashed disk itself.
fn run(config: TreeConfig, seed: u64, rounds: usize, ops: usize) {
    let mut rng = Rng(seed | 1);
    let store = FaultStore::new(&config);
    let mut tree = Tree::create(config, Box::new(store.clone())).unwrap();
    let mut store = store;
    let mut log = Log::default();
    for round in 0..rounds {
        let context = format!("seed {seed:#x} round {round}");
        // Writes and checkpoints, then a crash in one of three ways.
        let crash_at = rng.below(ops as u64) as usize;
        let mut last_commit = store.durable_state();
        for step in 0..ops {
            if step == crash_at {
                match rng.below(3) {
                    0 => {}
                    1 => store.die_after(rng.below(60)),
                    _ => store.fail_next_commit(if rng.below(2) == 0 {
                        CommitFault::BeforeCopy
                    } else {
                        CommitFault::AfterCopy
                    }),
                }
            }
            if rng.below(25) == 0 {
                if let Ok(state) = tree.checkpoint() {
                    last_commit = state;
                }
            } else {
                apply(&tree, &mut log, random_op(&mut rng, &config));
            }
            if step > crash_at && (tree.store_failed() || rng.below(8) == 0) {
                break;
            }
        }

        // Crash and recover.
        drop(tree);
        let crashed = store.crash(&mut rng);
        let (recovered, recovered_store) = recover(crashed, config, &mut rng);
        tree = recovered;
        store = recovered_store;
        let applied = tree.applied_seq();
        let durable = store.durable_state();
        assert_eq!(applied, durable.applied_seq, "{context}");
        assert!(
            applied >= last_commit.applied_seq,
            "{context}: an acknowledged commit was lost"
        );
        assert_eq!(contents(&tree), log.state_at(applied), "{context}");
        assert_eq!(tree.check_invariants(), Ok(()), "{context}");

        // Replay the lost operations: all, none, or a prefix. The log is
        // truncated to match, and replayed operations get new sequences.
        let lost: Vec<Op> = log
            .0
            .iter()
            .filter(|(seq, _)| *seq > applied)
            .map(|(_, op)| op.clone())
            .collect();
        log.0.retain(|(seq, _)| *seq <= applied);
        let replay = match rng.below(4) {
            0 => 0,
            1 => rng.below(lost.len() as u64 + 1) as usize,
            _ => lost.len(),
        };
        for op in lost.into_iter().take(replay) {
            assert!(apply(&tree, &mut log, op), "{context}: replay");
        }
        assert_eq!(contents(&tree), log.state_at(u64::MAX), "{context}");
    }
}

/// Tiny nodes and a circular buffer of about four pages, so most
/// write-throughs come from eviction, across many seeds.
#[test]
fn crashes_recover_the_committed_state_with_a_tiny_buffer() {
    let config = TreeConfig {
        circular_buffer_bytes: 1024,
        ..config(256, 128, 8, 16)
    };
    for seed in 0..24 {
        run(config, 0xc4a5 + seed * 7919, 12, 300);
    }
}

/// A larger circular buffer: write-throughs come from checkpoints and
/// merges, and full pages and caching are on.
#[test]
fn crashes_recover_the_committed_state_with_a_large_buffer() {
    let config = TreeConfig {
        circular_buffer_bytes: 64 << 10,
        write_load_full_page: true,
        read_promotion_percent: 20,
        ..config(256, 128, 8, 16)
    };
    for seed in 0..12 {
        run(config, 0x0b16 + seed * 104_729, 10, 400);
    }
}
