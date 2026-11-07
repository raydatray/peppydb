//! Commits of the write watermark, erasure on open, and store failures
//! (plan 0018).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::*;

fn small() -> TreeConfig {
    config(256, 128, 8, 16)
}

fn key(i: u32) -> [u8; 4] {
    i.to_be_bytes()
}

/// A memory store whose `sync` and `commit` fail while `fail` is set.
#[derive(Debug)]
struct FailingSync {
    inner: MemStore,
    fail: Arc<AtomicBool>,
}

fn io_error() -> StoreError {
    StoreError::Io {
        op: "sync",
        kind: std::io::ErrorKind::Other,
        code: Some(5),
    }
}

impl PageStore for FailingSync {
    fn page_size(&self) -> usize {
        self.inner.page_size()
    }
    fn page_count(&self) -> u64 {
        self.inner.page_count()
    }
    fn alloc(&self) -> Result<PageNumber, StoreError> {
        self.inner.alloc()
    }
    fn free(&self, page_number: PageNumber) -> Result<(), StoreError> {
        self.inner.free(page_number)
    }
    fn read(&self, page_number: PageNumber, buf: &mut [u8]) -> Result<(), StoreError> {
        self.inner.read(page_number, buf)
    }
    fn write(&self, page_number: PageNumber, buf: &[u8]) -> Result<(), StoreError> {
        self.inner.write(page_number, buf)
    }
    fn sync(&self) -> Result<(), StoreError> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(io_error());
        }
        self.inner.sync()
    }
    fn superblock(&self) -> &Superblock {
        self.inner.superblock()
    }
    fn state(&self) -> State {
        self.inner.state()
    }
    fn commit(&self, state: &State) -> Result<(), StoreError> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(io_error());
        }
        self.inner.commit(state)
    }
}

fn failing(config: TreeConfig) -> (Tree, Arc<AtomicBool>) {
    let fail = Arc::new(AtomicBool::new(false));
    let store = FailingSync {
        inner: MemStore::new(Superblock::from(&config)).unwrap(),
        fail: fail.clone(),
    };
    (Tree::create(config, Box::new(store)).unwrap(), fail)
}

#[test]
fn create_commits_an_empty_tree() {
    let tree = Tree::in_memory(small()).unwrap();
    assert_eq!(
        tree.store().state(),
        State {
            checkpoint_id: 1,
            applied_seq: 0,
            write_seq: 1,
        }
    );
    let tree = Tree::open(tree.into_store()).unwrap();
    assert_eq!(tree.iter().count(), 0);
}

#[test]
fn flush_commits_the_write_watermark() {
    let tree = Tree::in_memory(small()).unwrap();
    for i in 0..100 {
        tree.put(&key(i), b"v").unwrap();
    }
    tree.flush().unwrap();
    let state = tree.store().state();
    assert_eq!(state.checkpoint_id, 2);
    assert_eq!(state.write_seq, tree.leaves.write_seq());
    assert!(state.write_seq > 1);
}

#[test]
fn a_page_written_after_the_commit_is_erased_on_open() {
    let tree = Tree::in_memory(small()).unwrap();
    tree.put(&key(1), b"committed").unwrap();
    tree.flush().unwrap();
    // Write the leaf through again without a commit: a crash after the page
    // landed.
    tree.put(&key(1), b"uncommitted").unwrap();
    let mut bytes = tree.leaves.page_copy(LeafId::new(0).unwrap()).unwrap();
    let id = LeafId::new(0).unwrap();
    let leaf = tree.leaves.write(id).unwrap();
    drop(leaf);
    tree.leaves.overwrite(id, &mut bytes).unwrap();
    let late = tree.leaves.page_of(id).unwrap();
    let tree = Tree::open(tree.into_store()).unwrap();
    assert_eq!(tree.get(&key(1)), Ok(Some(b"committed".to_vec())));
    let mut erased = vec![1; 256];
    tree.store().read(late, &mut erased).unwrap();
    assert!(
        erased.iter().all(|&byte| byte == 0),
        "the late page is zeroed"
    );
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn a_crash_between_commit_and_free_reopens_cleanly() {
    let tree = Tree::in_memory(small()).unwrap();
    for i in 0..200 {
        tree.put(&key(i), b"v").unwrap();
    }
    // Commit by hand without the free step.
    let id = LeafId::new(0).unwrap();
    let mut bytes = tree.leaves.page_copy(id).unwrap();
    tree.leaves.overwrite(id, &mut bytes).unwrap();
    let written = tree.leaves.write_watermark();
    let current = tree.store().state();
    tree.store()
        .commit(&State {
            checkpoint_id: current.checkpoint_id + 1,
            write_seq: written,
            ..current
        })
        .unwrap();
    let pending = tree.leaves.take_superseded_through(written).len();
    assert!(pending > 0, "superseded pages were never freed");
    let pages = tree.store().page_count();

    let tree = Tree::open(tree.into_store()).unwrap();
    assert_eq!(tree.check_invariants(), Ok(()));
    // Open released the superseded copies: allocations reuse them first.
    for _ in 0..pending {
        assert!(tree.store().alloc().unwrap().raw() < pages);
    }
    assert_eq!(tree.store().page_count(), pages);
}

#[test]
fn a_write_through_in_flight_holds_back_the_commit() {
    super::concurrent::with_watchdog(Duration::from_secs(30), || {
        let tree = Arc::new(Tree::in_memory(small()).unwrap());
        let done = Arc::new(AtomicBool::new(false));
        let checkpoint = std::thread::scope(|scope| {
            let gate = tree.leaves.hold_gate();
            let handle = scope.spawn(|| {
                tree.flush().unwrap();
                done.store(true, Ordering::Release);
            });
            std::thread::sleep(Duration::from_millis(50));
            assert!(!done.load(Ordering::Acquire), "the commit waits");
            drop(gate);
            handle.join().unwrap();
            done.load(Ordering::Acquire)
        });
        assert!(checkpoint);
    });
}

#[test]
fn a_failed_commit_keeps_the_previous_state_and_fails_later_checkpoints() {
    let (tree, fail) = failing(small());
    tree.put(&key(1), b"v").unwrap();
    tree.flush().unwrap();
    let before = tree.store().state();

    fail.store(true, Ordering::Relaxed);
    tree.put(&key(2), b"w").unwrap();
    assert!(matches!(tree.flush(), Err(TreeError::Store(_))));
    assert_eq!(tree.store().state(), before);
    assert!(tree.store_failed());

    // The device recovers, but nothing later can be trusted.
    fail.store(false, Ordering::Relaxed);
    assert_eq!(tree.flush(), Err(TreeError::CheckpointFailed));
    assert_eq!(tree.put(&key(3), b"x"), Err(TreeError::StoreFailed));
    assert_eq!(tree.remove(&key(1)), Err(TreeError::StoreFailed));
    assert_eq!(tree.get(&key(1)), Ok(Some(b"v".to_vec())), "reads continue");

    // Reopening clears the flag and returns to the last commit.
    let tree = Tree::open(tree.into_store()).unwrap();
    assert!(!tree.store_failed());
    assert_eq!(tree.get(&key(1)), Ok(Some(b"v".to_vec())));
    assert_eq!(tree.get(&key(2)), Ok(None));
    tree.put(&key(3), b"x").unwrap();
    tree.flush().unwrap();
}

#[test]
fn accept_writes_keeps_writes_after_a_failure() {
    let (tree, fail) = failing(TreeConfig {
        on_store_failure: StoreFailurePolicy::AcceptWrites,
        ..small()
    });
    fail.store(true, Ordering::Relaxed);
    assert!(tree.flush().is_err());
    fail.store(false, Ordering::Relaxed);
    tree.put(&key(1), b"v").unwrap();
    assert_eq!(tree.get(&key(1)), Ok(Some(b"v".to_vec())));
    assert_eq!(tree.flush(), Err(TreeError::CheckpointFailed));
}

/// Plan 0018's erase argument: a page written after a checkpoint must not
/// come back after a later commit covers its sequence.
#[test]
fn an_uncommitted_page_does_not_return_after_a_later_commit() {
    let tree = Tree::in_memory(small()).unwrap();
    // Two leaves: L holds key 1, M holds the high keys.
    let mut i = 0;
    while tree.leaf_count() < 2 {
        tree.put(&key(i), &[1; 16]).unwrap();
        i += 1;
    }
    tree.flush().unwrap();
    let low = tree.find_leaf(&key(0));
    let high = tree.find_leaf(&key(i - 1));
    assert_ne!(low, high);

    // Crash after L's new page landed, uncommitted.
    tree.put(&key(0), b"lost").unwrap();
    let mut bytes = tree.leaves.page_copy(low).unwrap();
    tree.leaves.overwrite(low, &mut bytes).unwrap();
    let tree = Tree::open(tree.into_store()).unwrap();
    assert_eq!(tree.get(&key(0)), Ok(Some(vec![1; 16])));

    // Write only M, commit, crash, open: L stays at its committed state.
    for _ in 0..4 {
        tree.put(&key(i - 1), b"m").unwrap();
        tree.flush().unwrap();
    }
    let tree = Tree::open(tree.into_store()).unwrap();
    assert_eq!(tree.get(&key(0)), Ok(Some(vec![1; 16])));
    assert_eq!(tree.get(&key(i - 1)), Ok(Some(b"m".to_vec())));
    assert_eq!(tree.check_invariants(), Ok(()));
}
