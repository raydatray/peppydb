//! The leaf mapping table and write-through to the store.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use bf_tree_store::FileStore;

fn leaf(raw: u64) -> LeafId {
    LeafId::new(raw).unwrap()
}

#[test]
fn entry_lock_is_at_most_64_bytes() {
    assert!(size_of::<bf_tree_sync::RwLock<crate::leaf_map::LeafEntry>>() <= 64);
}

#[test]
fn a_new_tree_stores_its_one_leaf_at_once() {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    assert_eq!(tree.leaf_count(), 1);
    assert_eq!(tree.leaves.store().page_count(), 1);
    let stored = stored_pages(&tree);
    let page = Page::validate(&stored[0]).unwrap();
    assert!(page.is_empty());
    assert_eq!(
        page.fences(),
        Some(Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        })
    );
}

#[test]
fn a_fresh_map_over_the_store_serves_every_leaf() {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    for i in 0..200u32 {
        tree.put(&i.to_be_bytes(), b"v").unwrap();
    }
    // Merge every buffered record, so that the store holds every page.
    tree.flush().unwrap();
    let leaves = tree.leaf_count();
    assert!(leaves > 2);
    assert_eq!(tree.leaves.store().page_count() as usize, leaves);
    assert_eq!(stored_pages(&tree), copied_pages(&tree));

    // Map the store afresh, keeping the inner nodes.
    let pages = tree.leaves.page_table();
    let store = tree.leaves.into_store();
    let tree = Tree::assemble(
        tree.config,
        tree.inner,
        LeafMap::for_config(store, pages, &tree.config).unwrap(),
    );
    let key = 7u32.to_be_bytes();
    assert_eq!(tree.get(&key), Ok(Some(b"v".to_vec())));
    assert_eq!(
        tree.check_invariants(),
        Ok(()),
        "the checker loads every leaf"
    );

    // A put is buffered, and reaches the store only through a merge.
    let before = stored_pages(&tree);
    tree.put(&key, b"w").unwrap();
    assert_eq!(stored_pages(&tree), before);
    assert_eq!(tree.get(&key), Ok(Some(b"w".to_vec())));
    tree.flush().unwrap();
    let after = stored_pages(&tree);
    let changed = (0..leaves).filter(|&i| before[i] != after[i]).count();
    assert_eq!(changed, 1);
    assert_eq!(tree.get(&key), Ok(Some(b"w".to_vec())));
}

#[test]
fn unknown_and_freed_leaves_are_rejected_without_panicking() {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    assert_eq!(
        tree.leaves.page_copy(leaf(5)).err(),
        Some(TreeError::UnknownLeaf(leaf(5)))
    );
    assert_eq!(
        tree.leaves.overwrite(leaf(5), &mut [0; 256]).err(),
        Some(TreeError::UnknownLeaf(leaf(5)))
    );

    let store = tree.leaves.into_store();
    let tree = Tree::assemble(
        tree.config,
        tree.inner,
        LeafMap::for_config(store, [None], &tree.config).unwrap(),
    );
    assert_eq!(
        tree.leaves.page_copy(leaf(0)).err(),
        Some(TreeError::UnknownLeaf(leaf(0)))
    );
    assert_eq!(tree.get(b"a"), Err(TreeError::UnknownLeaf(leaf(0))));
}

#[test]
fn corrupt_stored_leaves_are_reported_on_load() {
    let config = config(256, 128, 8, 16);
    let store = MemStore::new(Superblock::from(&config)).unwrap();
    let page_number = store.alloc().unwrap();
    let id = leaf(0);
    let mut bytes = vec![0; 256];
    Page::init_with_fences(
        &mut bytes,
        Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        },
    )
    .unwrap();
    bytes[2..4].copy_from_slice(&512u16.to_le_bytes());
    stamp(&mut bytes, 0);
    store.write(page_number, &bytes).unwrap();

    let tree = Tree::assemble(
        config,
        Inner::new(config.inner_capacity, ChildRef::Leaf(id)),
        LeafMap::for_config(Box::new(store), [Some(page_number)], &config).unwrap(),
    );
    assert_eq!(
        tree.get(b"a"),
        Err(TreeError::CorruptLeaf {
            id,
            reason: "capacity field differs from the buffer length",
        })
    );
}

#[test]
fn a_split_loads_the_left_leaf_from_the_store() {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    for i in 0..7u32 {
        tree.put(&i.to_be_bytes(), &[1; 16]).unwrap();
    }
    tree.flush().unwrap();
    // Map the store of the nearly full leaf afresh, then force a split.
    let pages = tree.leaves.page_table();
    let store = tree.leaves.into_store();
    let tree = Tree::assemble(
        tree.config,
        tree.inner,
        LeafMap::for_config(store, pages, &tree.config).unwrap(),
    );

    tree.put(&7u32.to_be_bytes(), &[1; 16]).unwrap();
    tree.put(&8u32.to_be_bytes(), &[1; 16]).unwrap();
    assert_eq!(tree.height(), 1, "both puts are buffered");
    // The merge loads the base page from the store and does not fit it.
    tree.flush().unwrap();
    assert_eq!(tree.height(), 2);
    assert_eq!(tree.leaf_count(), 2);
    assert_eq!(stored_pages(&tree), copied_pages(&tree));
    assert_eq!(tree.check_invariants(), Ok(()));
}

/// A store that counts reads and syncs and records writes, over any
/// backend.
#[derive(Debug)]
pub(super) struct Counting {
    inner: Box<dyn PageStore>,
    io: Io,
}

/// Shared handles to a [`Counting`] store's counters.
#[derive(Clone, Debug, Default)]
pub(super) struct Io {
    reads: Counter,
    /// The leaf of every store write, in order.
    writes: Writes,
    syncs: Counter,
}

impl Io {
    /// Reads and writes since the last call, which resets them. Syncs are
    /// cumulative.
    pub(super) fn take(&self) -> (usize, Vec<LeafId>) {
        (
            self.reads.swap(0, Ordering::Relaxed),
            std::mem::take(&mut *self.writes.lock().unwrap()),
        )
    }

    fn syncs(&self) -> usize {
        self.syncs.load(Ordering::Relaxed)
    }
}

impl PageStore for Counting {
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
        self.io.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read(page_number, buf)
    }
    fn write(&self, page_number: PageNumber, buf: &[u8]) -> Result<(), StoreError> {
        // Record the leaf the page belongs to, from its stamp.
        let leaf = <Page as zerocopy::FromBytes>::ref_from_bytes(buf)
            .ok()
            .and_then(Page::leaf)
            .expect("written pages are stamped");
        self.io.writes.lock().unwrap().push(leaf);
        self.inner.write(page_number, buf)
    }
    fn sync(&self) -> Result<(), StoreError> {
        self.io.syncs.fetch_add(1, Ordering::Relaxed);
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

type Counter = Arc<AtomicUsize>;
type Writes = Arc<Mutex<Vec<LeafId>>>;

pub(super) fn counting(config: TreeConfig) -> (Box<dyn PageStore>, Io) {
    let io = Io::default();
    let store = Counting {
        inner: Box::new(MemStore::new(Superblock::from(&config)).unwrap()),
        io: io.clone(),
    };
    (Box::new(store), io)
}

/// Every leaf's stored page, read back through the store by its page
/// number; all zeros for a leaf with no stored page.
pub(super) fn stored_pages(tree: &Tree) -> Vec<Vec<u8>> {
    let store = tree.store();
    (0..tree.leaf_count())
        .map(|index| {
            let mut page = vec![0; store.page_size()];
            if let Some(page_number) = tree.leaves.page_of(leaf(index as u64)) {
                store.read(page_number, &mut page).unwrap();
            }
            page
        })
        .collect()
}

/// Every leaf page, copied under its shared lock.
fn copied_pages(tree: &Tree) -> Vec<Vec<u8>> {
    (0..tree.leaf_count())
        .map(|index| tree.leaves.page_copy(leaf(index as u64)).unwrap())
        .collect()
}

#[test]
fn create_checks_the_store_against_the_configuration() {
    let config = config(256, 128, 8, 16);
    let other = super::config(256, 128, 8, 8);
    let store = MemStore::new(Superblock::from(&other)).unwrap();
    assert_eq!(
        Tree::create(config, Box::new(store)).err(),
        Some(TreeError::InvalidStore(
            "store superblock differs from the configuration"
        ))
    );

    let used = MemStore::new(Superblock::from(&config)).unwrap();
    used.alloc().unwrap();
    assert_eq!(
        Tree::create(config, Box::new(used)).err(),
        Some(TreeError::InvalidStore("store already holds pages"))
    );

    let bad = super::config(64, 64, 1, 0);
    let store = MemStore::new(Superblock::from(&bad)).unwrap();
    assert!(matches!(
        Tree::create(bad, Box::new(store)),
        Err(TreeError::InvalidConfig(_))
    ));

    let store = MemStore::new(Superblock::from(&config)).unwrap();
    let tree = Tree::create(config, Box::new(store)).unwrap();
    assert_eq!(tree.store().page_count(), 1);
    assert!(tree.leaves.is_live(leaf(0)));
    assert_eq!(tree.height(), 1);
}

#[test]
fn blind_writes_touch_the_store_only_through_merges() {
    let config = config(256, 128, 8, 16);
    let (store, io) = counting(config);
    let tree = Tree::create(config, store).unwrap();
    assert_eq!(io.take(), (0, vec![leaf(0)]), "create writes the root");

    // A put to a leaf without a mini-page allocates one: no store I/O.
    tree.put(&0u32.to_be_bytes(), b"v").unwrap();
    assert_eq!(io.take(), (0, vec![]));

    let (mut buffered, mut in_place, mut splits) = (0, 0, 0);
    // A stride coprime with 200 spreads the puts over every leaf.
    for i in 1..200u32 {
        let key = (i * 7 % 200).to_be_bytes();
        let (left, _) = tree.locate(&key);
        let leaves = tree.leaf_count();
        tree.put(&key, b"v").unwrap();
        match io.take() {
            // The mini-page had room.
            (0, written) => {
                assert!(written.is_empty(), "put {i}: {written:?}");
                assert_eq!(tree.leaf_count(), leaves);
                buffered += 1;
            }
            // The mini-page needed a merge: one read of the base page, then
            // one write in place.
            (1, written) if tree.leaf_count() == leaves => {
                assert_eq!(written, [left], "put {i}");
                in_place += 1;
            }
            // A split: the new right leaf first, then the reachable left one.
            (1, written) => {
                assert_eq!(tree.leaf_count(), leaves + 1);
                assert_eq!(written, [leaf(leaves as u64), left], "put {i}");
                splits += 1;
            }
            other => panic!("put {i}: {other:?}"),
        }
        // Either way, the record ends up buffered.
        assert!(tree.leaves.mini_copy(tree.find_leaf(&key)).is_some());
    }
    assert!(
        buffered > in_place + splits,
        "{buffered} {in_place} {splits}"
    );
    assert!(in_place >= 1 && splits >= 2, "{in_place} {splits}");
    assert_eq!(io.syncs(), 0);

    // Flush merges each buffered leaf with one read and one or two writes,
    // then syncs once.
    let merges = tree.buffered_leaves();
    assert!(merges > 1);
    let leaves = tree.leaf_count();
    io.take();
    tree.flush().unwrap();
    let (reads, written) = io.take();
    assert_eq!(reads, merges, "one read per merge");
    let new = tree.leaf_count() - leaves;
    assert_eq!(written.len(), merges + new, "{written:?}");
    for (index, id) in written.iter().enumerate() {
        if id.index() >= leaves {
            // A new right leaf is written just before its left leaf.
            assert!(written[index + 1].index() < leaves, "{written:?}");
        }
    }
    assert_eq!(io.syncs(), 1);
    assert_eq!(tree.buffered_leaves(), 0);
    assert_eq!(tree.circular_buffer_used(), 0);
    assert_eq!(stored_pages(&tree), copied_pages(&tree));

    // A flush with nothing buffered only syncs.
    io.take();
    tree.flush().unwrap();
    assert_eq!(io.take(), (0, vec![]));
    assert_eq!(io.syncs(), 2);

    // Reads: a key the leaf does not buffer reads the base page once; a
    // buffered record or tombstone needs no read. Writes and rejected
    // operations do no I/O.
    let (key3, key4) = (3u32.to_be_bytes(), 4u32.to_be_bytes());
    let (owner, _) = tree.locate(&key3);
    assert_eq!(tree.find_leaf(&key4), owner);
    assert_eq!(tree.get(&key3), Ok(Some(b"v".to_vec())));
    assert_eq!(io.take(), (1, vec![]), "unbuffered get");
    assert_eq!(tree.put(&[1; 9], b"v"), Err(TreeError::KeyTooLarge));
    assert_eq!(io.take(), (0, vec![]), "rejected put");
    assert_eq!(tree.put(&key3, b"w"), Ok(()));
    assert_eq!(io.take(), (0, vec![]), "buffered put");
    assert_eq!(tree.get(&key3), Ok(Some(b"w".to_vec())));
    assert_eq!(io.take(), (0, vec![]), "get of a buffered record");
    assert_eq!(tree.remove(&key3), Ok(()));
    assert_eq!(io.take(), (0, vec![]), "buffered remove");
    assert_eq!(tree.get(&key3), Ok(None));
    assert_eq!(io.take(), (0, vec![]), "get of a tombstone");
    assert_eq!(tree.get(&key4), Ok(Some(b"v".to_vec())));
    assert_eq!(io.take(), (1, vec![]), "get the mini-page misses");

    tree.flush().unwrap();
    assert_eq!(io.take(), (1, vec![owner]), "one merge in place");
    assert_eq!(io.syncs(), 3);
    assert_eq!(tree.get(&key3), Ok(None));
    assert_eq!(stored_pages(&tree), copied_pages(&tree));
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn only_flushed_writes_survive_closing_the_tree() {
    let config = config(256, 128, 8, 16);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.bf");
    let store = FileStore::create(&path, Superblock::from(&config)).unwrap();
    let tree = Tree::create(config, Box::new(store)).unwrap();
    let write_all = |tree: &Tree| {
        for i in 0..300u32 {
            tree.put(&i.to_be_bytes(), &i.to_le_bytes()).unwrap();
        }
        for i in (0..300u32).step_by(7) {
            tree.remove(&i.to_be_bytes()).unwrap();
        }
    };

    // Without a flush, closing the tree loses the buffered writes. The
    // merged ones are in the store, which still opens as a valid tree.
    write_all(&tree);
    assert!(tree.buffered_leaves() > 0);
    let live = collect(tree.iter());
    let tree = Tree::open(tree.into_store()).unwrap();
    assert_eq!(tree.check_invariants(), Ok(()));
    let survived = collect(tree.iter());
    assert_ne!(survived, live);
    for (key, value) in survived {
        let i = u32::from_be_bytes(key.try_into().unwrap());
        assert_eq!(value, i.to_le_bytes());
    }

    // After a flush, every write survives.
    write_all(&tree);
    tree.flush().unwrap();
    assert!(tree.height() >= 2);
    let leaves = tree.leaf_count();
    let expected = collect(tree.iter());
    assert_eq!(expected.len(), 300 - 300usize.div_ceil(7));
    // Writes after the flush stay buffered in fresh mini-pages, and are lost.
    tree.put(&1000u32.to_be_bytes(), b"lost").unwrap();
    tree.remove(&1u32.to_be_bytes()).unwrap();

    let tree = Tree::open(tree.into_store()).unwrap();
    assert_eq!(tree.leaf_count(), leaves);
    assert_eq!(collect(tree.iter()), expected);
    assert_eq!(tree.check_invariants(), Ok(()));
    drop(tree);

    // The same holds through the file.
    let tree = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    assert_eq!(collect(tree.iter()), expected);
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn memory_and_file_backends_store_identical_bytes() {
    let config = config(256, 128, 8, 16);
    let dir = tempfile::tempdir().unwrap();
    let file = FileStore::create(dir.path().join("tree.bf"), Superblock::from(&config)).unwrap();
    let mem = MemStore::new(Superblock::from(&config)).unwrap();
    let mut trees = [
        Tree::create(config, Box::new(mem)).unwrap(),
        Tree::create(config, Box::new(file)).unwrap(),
    ];

    for tree in &mut trees {
        for i in 0..300u32 {
            tree.put(&i.to_be_bytes(), &i.to_le_bytes()).unwrap();
            if i % 3 == 0 {
                tree.remove(&(i / 2).to_be_bytes()).unwrap();
            }
            if i % 97 == 0 {
                tree.flush().unwrap();
            }
        }
        tree.flush().unwrap();
        assert_eq!(tree.check_invariants(), Ok(()));
    }

    let [mem, file] = trees;
    assert_eq!(stored_pages(&mem), stored_pages(&file));
    assert_eq!(stored_pages(&file), copied_pages(&file));
    assert_eq!(collect(mem.iter()), collect(file.iter()));
    assert!(mem.height() >= 2);
}

#[test]
fn create_rejects_a_store_with_a_committed_state() {
    let config = config(256, 128, 8, 16);
    let store = MemStore::new(Superblock::from(&config)).unwrap();
    store
        .commit(&State {
            checkpoint_id: 1,
            ..State::default()
        })
        .unwrap();
    assert_eq!(
        Tree::create(config, Box::new(store)).err(),
        Some(TreeError::InvalidStore("store already holds pages"))
    );
}

/// Every live leaf's stored page passes its checksum and names that leaf.
fn assert_stamped(tree: &Tree) {
    let store = tree.store();
    for (index, page_number) in tree.leaves.page_table().into_iter().enumerate() {
        let Some(page_number) = page_number else {
            continue;
        };
        let mut bytes = vec![0; store.page_size()];
        store.read(page_number, &mut bytes).unwrap();
        assert!(bf_tree_page::verify_stored_checksum(&bytes), "leaf {index}");
        let page = Page::validate(&bytes).unwrap();
        assert_eq!(page.leaf(), Some(leaf(index as u64)));
        assert_eq!(page.backing_leaf(), None, "base pages overlay nothing");
    }
}

#[test]
fn reopen_rebuilds_the_page_table_from_the_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pages.bf");
    let config = config(256, 128, 8, 16);
    let store = FileStore::create(&path, Superblock::from(&config)).unwrap();
    let tree = Tree::create(config, Box::new(store)).unwrap();
    let mut oracle = std::collections::BTreeMap::new();
    for i in 0..600u32 {
        let key = i.wrapping_mul(2_654_435_761).to_be_bytes();
        tree.put(&key, &[i as u8; 12]).unwrap();
        oracle.insert(key.to_vec(), vec![i as u8; 12]);
    }
    tree.flush().unwrap();
    assert!(tree.leaf_count() > 10);
    assert_stamped(&tree);
    let before = tree.leaves.page_table();
    drop(tree);

    let tree = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    assert_eq!(tree.leaves.page_table(), before);
    assert_stamped(&tree);
    let all: Vec<_> = tree.iter().map(Result::unwrap).collect();
    assert_eq!(all, oracle.into_iter().collect::<Vec<_>>());
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn leaf_ids_stop_at_the_32_bit_stamp_limit() {
    // Only the rejection is tested: allocating an ID near the limit would
    // grow the leaf table to billions of entries. The check runs before
    // any table grows.
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    tree.leaves.set_next_leaf(u32::MAX as usize);
    assert_eq!(tree.leaves.alloc().err(), Some(TreeError::TooManyLeaves));
    assert_eq!(tree.leaves.alloc().err(), Some(TreeError::TooManyLeaves));
    assert_eq!(tree.leaves.len(), 1, "nothing grew");
}
