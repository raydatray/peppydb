//! Eviction: a full circular buffer makes room instead of failing.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use bf_tree_circular_buffer::Claim;

/// A circular buffer of two leaf pages: 2 * (256 + 16) rounds up to 1 KiB.
fn tiny_buffer() -> TreeConfig {
    TreeConfig {
        circular_buffer_bytes: 1024,
        ..config(256, 128, 8, 16)
    }
}

fn key(i: u32) -> [u8; 4] {
    i.wrapping_mul(0x9e37_79b9).to_be_bytes()
}

#[test]
fn thousands_of_writes_fit_a_two_page_buffer() {
    let config = tiny_buffer();
    let tree = Tree::in_memory(config).unwrap();
    let mut oracle = BTreeMap::new();
    for i in 0..5000u32 {
        let value = [(i % 251) as u8; 6];
        tree.put(&key(i % 1500), &value).unwrap();
        oracle.insert(key(i % 1500).to_vec(), value.to_vec());
        if i % 7 == 0 {
            tree.remove(&key((i * 3) % 1500)).unwrap();
            oracle.remove(key((i * 3) % 1500).as_slice());
        }
        assert!(tree.circular_buffer_used() <= config.circular_buffer_bytes);
    }
    assert!(tree.height() >= 3, "evictions split leaves");
    assert_eq!(tree.check_invariants(), Ok(()));
    let all: Vec<_> = tree.iter().collect::<Result<_, _>>().unwrap();
    assert_eq!(all, oracle.clone().into_iter().collect::<Vec<_>>());

    tree.flush().unwrap();
    let reopened = Tree::open(tree.into_store()).unwrap();
    let all: Vec<_> = reopened.iter().collect::<Result<_, _>>().unwrap();
    assert_eq!(all, oracle.into_iter().collect::<Vec<_>>());
}

#[test]
fn a_stale_claim_is_retired_without_a_merge() {
    let tree = Tree::in_memory(tiny_buffer()).unwrap();
    tree.put(b"a", b"1").unwrap();
    let Claim::Victim(victim) = tree.leaves.circular_buffer.claim_victim() else {
        panic!("one live block");
    };
    // Grow the claimed block: the write copies it to a larger class and
    // frees the claimed one, which stays claimed.
    for i in 0..4u8 {
        tree.put(&[b'b', i], &[i; 12]).unwrap();
    }
    let base_before = tree.leaves.page_copy(LeafId::new(0).unwrap()).unwrap();
    tree.evict(victim).unwrap();
    // No merge happened: the base page is unchanged, every record buffered.
    assert_eq!(
        tree.leaves.page_copy(LeafId::new(0).unwrap()).unwrap(),
        base_before
    );
    assert_eq!(tree.buffered_leaves(), 1);
    assert_eq!(tree.get(b"a"), Ok(Some(b"1".to_vec())));
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn an_eviction_whose_merge_does_not_fit_splits_the_leaf() {
    let tree = Tree::in_memory(tiny_buffer()).unwrap();
    // Fill the base page, then buffer more than it can absorb.
    for i in 0..8u8 {
        tree.put(&[b'k', i * 2], &[1; 12]).unwrap();
    }
    tree.flush().unwrap();
    for i in 0..4u8 {
        tree.put(&[b'k', i * 2 + 1], &[2; 12]).unwrap();
    }
    assert_eq!(tree.height(), 1);
    assert_eq!(tree.evict_one(), Ok(crate::evict::Eviction::Evicted));
    assert_eq!(tree.height(), 2);
    assert_eq!(tree.buffered_leaves(), 0);
    assert_eq!(tree.iter().count(), 12);
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn a_writer_whose_own_leaf_is_the_oldest_block_evicts_it() {
    let tree = Tree::in_memory(tiny_buffer()).unwrap();
    // One leaf: every block is this leaf's, so each eviction merges the
    // writer's own leaf, with no lock held.
    for i in 0..300u16 {
        tree.put(&i.to_be_bytes(), &[3; 10]).unwrap();
    }
    assert_eq!(tree.iter().count(), 300);
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn busy_and_empty_claims() {
    let tree = Tree::in_memory(tiny_buffer()).unwrap();
    assert_eq!(tree.evict_one(), Ok(crate::evict::Eviction::Empty));
    tree.put(b"a", b"1").unwrap();
    let Claim::Victim(victim) = tree.leaves.circular_buffer.claim_victim() else {
        panic!("one live block");
    };
    assert_eq!(tree.evict_one(), Ok(crate::evict::Eviction::Busy));
    tree.evict(victim).unwrap();
    assert_eq!(tree.evict_one(), Ok(crate::evict::Eviction::Empty));
    assert_eq!(tree.get(b"a"), Ok(Some(b"1".to_vec())));
}

/// A store that fails writes while `fail` is set.
#[derive(Debug)]
struct Failing {
    inner: MemStore,
    fail: Arc<AtomicBool>,
}

impl PageStore for Failing {
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
        if self.fail.load(Ordering::Relaxed) {
            return Err(StoreError::Io {
                op: "write",
                kind: std::io::ErrorKind::Other,
                code: None,
            });
        }
        self.inner.write(page_number, buf)
    }
    fn sync(&self) -> Result<(), StoreError> {
        self.inner.sync()
    }
    fn superblock(&self) -> &Superblock {
        self.inner.superblock()
    }
    fn state(&self) -> State {
        self.inner.state()
    }
    fn commit(&self, state: &State) -> Result<(), StoreError> {
        self.inner.commit(state)
    }
}

#[test]
fn a_store_error_during_eviction_returns_the_claim_and_keeps_the_records() {
    let config = tiny_buffer();
    let fail = Arc::new(AtomicBool::new(false));
    let store = Failing {
        inner: MemStore::new(Superblock::from(&config)).unwrap(),
        fail: fail.clone(),
    };
    let tree = Tree::create(config, Box::new(store)).unwrap();
    tree.put(b"a", b"1").unwrap();
    tree.put(b"b", b"2").unwrap();

    fail.store(true, Ordering::Relaxed);
    assert!(matches!(tree.evict_one(), Err(TreeError::Store(_))));
    // The claim was returned: the block is live and evictable again.
    assert_eq!(tree.buffered_leaves(), 1);
    assert_eq!(tree.get(b"a"), Ok(Some(b"1".to_vec())));
    assert_eq!(tree.get(b"b"), Ok(Some(b"2".to_vec())));

    fail.store(false, Ordering::Relaxed);
    assert_eq!(tree.evict_one(), Ok(crate::evict::Eviction::Evicted));
    assert_eq!(tree.buffered_leaves(), 0);
    assert_eq!(tree.get(b"a"), Ok(Some(b"1".to_vec())));
    assert_eq!(tree.check_invariants(), Ok(()));
}
