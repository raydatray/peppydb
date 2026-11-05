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
    assert_eq!(tree.leaves.store().slots(), 1);
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
    let leaves = tree.leaf_count();
    assert!(leaves > 2);
    assert_eq!(tree.leaves.store().slots() as usize, leaves);
    assert_eq!(stored_pages(&tree), copied_pages(&tree));

    // Map the unflushed store afresh, keeping the inner nodes: the store
    // already holds every page.
    let store = tree.leaves.into_store();
    let tree = Tree::assemble(
        tree.config,
        tree.inner.into_inner(),
        LeafMap::for_config(store, (0..leaves).map(|_| false), &tree.config).unwrap(),
    );
    let key = 7u32.to_be_bytes();
    assert_eq!(tree.get(&key), Ok(Some(b"v".to_vec())));
    assert_eq!(
        tree.check_invariants(),
        Ok(()),
        "the checker loads every leaf"
    );

    // A put reaches the store before it returns.
    let before = stored_pages(&tree);
    tree.put(&key, b"w").unwrap();
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
        tree.inner.into_inner(),
        LeafMap::for_config(store, [true], &tree.config).unwrap(),
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
    let id = store.alloc().unwrap();
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
    store.write(id, &bytes).unwrap();

    let tree = Tree::assemble(
        config,
        InnerStore::new(config.inner_capacity, ChildRef::Leaf(id)),
        LeafMap::for_config(Box::new(store), [false], &config).unwrap(),
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
    // Map the store of the full leaf afresh, then force a split.
    let store = tree.leaves.into_store();
    let tree = Tree::assemble(
        tree.config,
        tree.inner.into_inner(),
        LeafMap::for_config(store, [false], &tree.config).unwrap(),
    );

    tree.put(&7u32.to_be_bytes(), &[1; 16]).unwrap();
    tree.put(&8u32.to_be_bytes(), &[1; 16]).unwrap();
    assert_eq!(tree.height(), 2);
    assert_eq!(tree.leaf_count(), 2);
    assert_eq!(stored_pages(&tree), copied_pages(&tree));
    assert_eq!(tree.check_invariants(), Ok(()));
}

/// A store that records writes and counts syncs, over any backend.
#[derive(Debug)]
struct Counting {
    inner: Box<dyn PageStore>,
    writes: Writes,
    syncs: Counter,
}

impl PageStore for Counting {
    fn page_size(&self) -> usize {
        self.inner.page_size()
    }
    fn slots(&self) -> u64 {
        self.inner.slots()
    }
    fn alloc(&self) -> Result<LeafId, StoreError> {
        self.inner.alloc()
    }
    fn free(&self, id: LeafId) -> Result<(), StoreError> {
        self.inner.free(id)
    }
    fn read(&self, id: LeafId, buf: &mut [u8]) -> Result<(), StoreError> {
        self.inner.read(id, buf)
    }
    fn write(&self, id: LeafId, buf: &[u8]) -> Result<(), StoreError> {
        self.writes.lock().unwrap().push(id);
        self.inner.write(id, buf)
    }
    fn sync(&self) -> Result<(), StoreError> {
        self.syncs.fetch_add(1, Ordering::Relaxed);
        self.inner.sync()
    }
    fn superblock(&self) -> &Superblock {
        self.inner.superblock()
    }
}

type Counter = Arc<AtomicUsize>;
/// The leaf of every store write, in order.
type Writes = Arc<Mutex<Vec<LeafId>>>;

fn counting(config: TreeConfig) -> (Box<dyn PageStore>, Writes, Counter) {
    let writes = Writes::default();
    let syncs = Arc::new(AtomicUsize::new(0));
    let store = Counting {
        inner: Box::new(MemStore::new(Superblock::from(&config)).unwrap()),
        writes: writes.clone(),
        syncs: syncs.clone(),
    };
    (Box::new(store), writes, syncs)
}

/// Every stored page, read back through the store.
fn stored_pages(tree: &Tree) -> Vec<Vec<u8>> {
    let store = tree.store();
    (0..store.slots())
        .map(|raw| {
            let mut page = vec![0; store.page_size()];
            store.read(leaf(raw), &mut page).unwrap();
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
    assert_eq!(tree.store().slots(), 1);
    assert!(tree.leaves.is_live(leaf(0)));
    assert_eq!(tree.height(), 1);
}

#[test]
fn puts_write_through_and_flush_only_syncs() {
    let config = config(256, 128, 8, 16);
    let (store, writes, syncs) = counting(config);
    let tree = Tree::create(config, store).unwrap();
    assert_eq!(*writes.lock().unwrap(), [leaf(0)], "create writes the root");
    writes.lock().unwrap().clear();

    let mut splits = 0;
    for i in 0..200u32 {
        let key = i.to_be_bytes();
        let (left, _) = tree.locate(&key);
        let leaves = tree.leaf_count();
        tree.put(&key, b"v").unwrap();
        let written = std::mem::take(&mut *writes.lock().unwrap());
        if tree.leaf_count() == leaves {
            // In place: one write of the owning leaf.
            assert_eq!(written, [left], "put {i}");
        } else {
            // A split: the new right leaf first, then the reachable left one.
            assert_eq!(tree.leaf_count(), leaves + 1);
            assert_eq!(written, [leaf(leaves as u64), left], "put {i}");
            splits += 1;
        }
    }
    assert!(splits >= 2);
    assert_eq!(syncs.load(Ordering::Relaxed), 0);
    assert_eq!(stored_pages(&tree), copied_pages(&tree));

    // Flush writes nothing and syncs once, every time.
    tree.flush().unwrap();
    tree.flush().unwrap();
    assert!(writes.lock().unwrap().is_empty());
    assert_eq!(syncs.load(Ordering::Relaxed), 2);

    // Reads and rejected operations write nothing; a remove writes its leaf.
    assert_eq!(tree.get(&3u32.to_be_bytes()), Ok(Some(b"v".to_vec())));
    assert_eq!(tree.put(&[1; 9], b"v"), Err(TreeError::KeyTooLarge));
    assert!(writes.lock().unwrap().is_empty());
    let (owner, _) = tree.locate(&3u32.to_be_bytes());
    assert_eq!(tree.remove(&3u32.to_be_bytes()), Ok(()));
    assert_eq!(*writes.lock().unwrap(), [owner]);
    assert_eq!(tree.get(&3u32.to_be_bytes()), Ok(None));
    assert_eq!(stored_pages(&tree), copied_pages(&tree));
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn a_dropped_tree_loses_no_write() {
    let config = config(256, 128, 8, 16);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.bf");
    let store = FileStore::create(&path, Superblock::from(&config)).unwrap();
    let tree = Tree::create(config, Box::new(store)).unwrap();
    for i in 0..300u32 {
        tree.put(&i.to_be_bytes(), &i.to_le_bytes()).unwrap();
    }
    for i in (0..300u32).step_by(7) {
        tree.remove(&i.to_be_bytes()).unwrap();
    }
    assert!(tree.height() >= 2);
    let leaves = tree.leaf_count();
    let expected = collect(tree.iter());

    // No flush: the store already holds every write.
    let tree = Tree::open(tree.into_store()).unwrap();
    assert_eq!(tree.leaf_count(), leaves);
    assert_eq!(collect(tree.iter()), expected);
    assert_eq!(tree.check_invariants(), Ok(()));
    drop(tree);

    // The same holds through the file, written but never synced.
    let tree = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    assert_eq!(collect(tree.iter()), expected);
    assert_eq!(expected.len(), 300 - 300usize.div_ceil(7));
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
