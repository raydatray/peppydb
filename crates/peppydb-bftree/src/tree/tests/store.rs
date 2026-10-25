//! The leaf mapping table: residency and dirty tracking.

use std::cell::Cell;
use std::rc::Rc;

use zerocopy::IntoBytes;

use super::*;
use crate::FileStore;

fn leaf(raw: u64) -> LeafId {
    LeafId::new(raw).unwrap()
}

#[test]
fn a_new_tree_has_one_dirty_resident_leaf() {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    assert_eq!(tree.leaf_count(), 1);
    assert_eq!(tree.resident_leaves(), 1);
    assert_eq!(tree.dirty_leaves(), 1);
    assert_eq!(tree.leaves.store().slots(), 1);
}

#[test]
fn reads_do_not_dirty_and_writes_do() {
    let mut tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    for i in 0..200u32 {
        tree.put(&i.to_be_bytes(), b"v").unwrap();
    }
    let leaves = tree.leaf_count();
    assert!(leaves > 2);
    assert_eq!(tree.resident_leaves(), leaves);
    assert_eq!(tree.dirty_leaves(), leaves, "every split output is dirty");
    assert_eq!(tree.leaves.store().slots() as usize, leaves);

    // Copy every page to the store by hand, then map the store afresh:
    // nothing is resident until touched.
    let images: Vec<_> = (0..leaves)
        .map(|index| tree.leaf(leaf(index as u64)).unwrap().as_bytes().to_vec())
        .collect();
    let mut store = tree.leaves.into_store();
    for (index, image) in images.iter().enumerate() {
        store.write(leaf(index as u64), image).unwrap();
    }
    let mut tree = Tree {
        config: tree.config,
        root: tree.root,
        inner: tree.inner,
        leaves: LeafMap::new(store, (0..leaves).map(|_| false)),
    };
    assert_eq!(tree.resident_leaves(), 0);
    assert_eq!(tree.dirty_leaves(), 0);

    let key = 7u32.to_be_bytes();
    assert_eq!(tree.get(&key), Ok(Some(b"v".as_slice())));
    assert_eq!(tree.resident_leaves(), 1);
    assert_eq!(tree.dirty_leaves(), 0);
    assert_eq!(
        tree.check_invariants(),
        Ok(()),
        "the checker loads every leaf"
    );
    assert_eq!(tree.resident_leaves(), leaves);
    assert_eq!(tree.dirty_leaves(), 0);

    tree.put(&key, b"w").unwrap();
    assert_eq!(tree.dirty_leaves(), 1);
}

#[test]
fn unknown_and_freed_leaves_are_rejected_without_panicking() {
    let mut tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    assert_eq!(
        tree.leaf(leaf(5)).err(),
        Some(TreeError::UnknownLeaf(leaf(5)))
    );
    assert_eq!(
        tree.leaf_mut(leaf(5)).err(),
        Some(TreeError::UnknownLeaf(leaf(5)))
    );

    let store = tree.leaves.into_store();
    let tree = Tree {
        config: tree.config,
        root: tree.root,
        inner: tree.inner,
        leaves: LeafMap::new(store, [true]),
    };
    assert_eq!(
        tree.leaf(leaf(0)).err(),
        Some(TreeError::UnknownLeaf(leaf(0)))
    );
    assert_eq!(tree.get(b"a"), Err(TreeError::UnknownLeaf(leaf(0))));
}

#[test]
fn corrupt_stored_leaves_are_reported_on_load() {
    let config = config(256, 128, 8, 16);
    let mut store = MemStore::new(Superblock::from_config(&config)).unwrap();
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

    let tree = Tree {
        config,
        root: ChildRef::Leaf(id),
        inner: InnerStore::new(config.inner_capacity),
        leaves: LeafMap::new(Box::new(store), [false]),
    };
    assert_eq!(
        tree.get(b"a"),
        Err(TreeError::CorruptLeaf {
            id,
            reason: "capacity field differs from the buffer length",
        })
    );
}

#[test]
fn split_pairs_load_the_left_leaf_on_demand() {
    let mut tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    for i in 0..7u32 {
        tree.put(&i.to_be_bytes(), &[1; 16]).unwrap();
    }
    // Round-trip the full leaf through the store, then force a split.
    let mut page = vec![0; 256];
    page.copy_from_slice(tree.leaf(leaf(0)).unwrap().as_bytes());
    let mut store = tree.leaves.into_store();
    store.write(leaf(0), &page).unwrap();
    let mut tree = Tree {
        config: tree.config,
        root: tree.root,
        inner: tree.inner,
        leaves: LeafMap::new(store, [false]),
    };
    assert_eq!(tree.resident_leaves(), 0);

    tree.put(&7u32.to_be_bytes(), &[1; 16]).unwrap();
    tree.put(&8u32.to_be_bytes(), &[1; 16]).unwrap();
    assert_eq!(tree.height(), 2);
    assert_eq!(tree.leaf_count(), 2);
    assert_eq!(tree.resident_leaves(), 2);
    assert_eq!(tree.dirty_leaves(), 2);
    assert_eq!(tree.check_invariants(), Ok(()));
}

/// A store that counts writes and syncs, over any backend.
struct Counting {
    inner: Box<dyn PageStore>,
    writes: Rc<Cell<usize>>,
    syncs: Rc<Cell<usize>>,
}

impl PageStore for Counting {
    fn page_size(&self) -> usize {
        self.inner.page_size()
    }
    fn slots(&self) -> u64 {
        self.inner.slots()
    }
    fn alloc(&mut self) -> Result<LeafId, StoreError> {
        self.inner.alloc()
    }
    fn free(&mut self, id: LeafId) -> Result<(), StoreError> {
        self.inner.free(id)
    }
    fn read(&self, id: LeafId, buf: &mut [u8]) -> Result<(), StoreError> {
        self.inner.read(id, buf)
    }
    fn write(&mut self, id: LeafId, buf: &[u8]) -> Result<(), StoreError> {
        self.writes.set(self.writes.get() + 1);
        self.inner.write(id, buf)
    }
    fn sync(&mut self) -> Result<(), StoreError> {
        self.syncs.set(self.syncs.get() + 1);
        self.inner.sync()
    }
    fn superblock(&self) -> &Superblock {
        self.inner.superblock()
    }
}

type Counter = Rc<Cell<usize>>;

fn counting(config: TreeConfig) -> (Box<dyn PageStore>, Counter, Counter) {
    let writes = Rc::new(Cell::new(0));
    let syncs = Rc::new(Cell::new(0));
    let store = Counting {
        inner: Box::new(MemStore::new(Superblock::from_config(&config)).unwrap()),
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

fn resident_pages(tree: &Tree) -> Vec<Vec<u8>> {
    (0..tree.leaf_count())
        .map(|index| tree.leaf(leaf(index as u64)).unwrap().as_bytes().to_vec())
        .collect()
}

#[test]
fn create_checks_the_store_against_the_configuration() {
    let config = config(256, 128, 8, 16);
    let other = super::config(256, 128, 8, 8);
    let store = MemStore::new(Superblock::from_config(&other)).unwrap();
    assert_eq!(
        Tree::create(config, Box::new(store)).err(),
        Some(TreeError::InvalidStore(
            "store superblock differs from the configuration"
        ))
    );

    let mut used = MemStore::new(Superblock::from_config(&config)).unwrap();
    used.alloc().unwrap();
    assert_eq!(
        Tree::create(config, Box::new(used)).err(),
        Some(TreeError::InvalidStore("store already holds pages"))
    );

    let bad = super::config(64, 64, 1, 0);
    let store = MemStore::new(Superblock::from_config(&bad)).unwrap();
    assert!(matches!(
        Tree::create(bad, Box::new(store)),
        Err(TreeError::InvalidConfig(_))
    ));

    let store = MemStore::new(Superblock::from_config(&config)).unwrap();
    let tree = Tree::create(config, Box::new(store)).unwrap();
    assert_eq!(tree.dirty_leaves(), 1);
    assert_eq!(tree.height(), 1);
}

#[test]
fn flush_writes_exactly_the_dirty_leaves_then_syncs() {
    let config = config(256, 128, 8, 16);
    let (store, writes, syncs) = counting(config);
    let mut tree = Tree::create(config, store).unwrap();
    assert_eq!(writes.get(), 0);

    for i in 0..200u32 {
        tree.put(&i.to_be_bytes(), b"v").unwrap();
    }
    let leaves = tree.leaf_count();
    assert_eq!(tree.dirty_leaves(), leaves);

    tree.flush().unwrap();
    assert_eq!((writes.get(), syncs.get()), (leaves, 1));
    assert_eq!(tree.dirty_leaves(), 0);
    assert_eq!(stored_pages(&tree), resident_pages(&tree));

    // A clean tree writes nothing; reads keep it clean.
    assert_eq!(tree.get(&3u32.to_be_bytes()), Ok(Some(b"v".as_slice())));
    tree.flush().unwrap();
    assert_eq!((writes.get(), syncs.get()), (leaves, 2));

    // One leaf written after one update; a split dirties two.
    tree.put(&3u32.to_be_bytes(), b"w").unwrap();
    assert_eq!(tree.dirty_leaves(), 1);
    tree.flush().unwrap();
    assert_eq!(writes.get(), leaves + 1);
    assert_eq!(stored_pages(&tree), resident_pages(&tree));

    assert_eq!(tree.remove(&3u32.to_be_bytes()), Ok(true));
    assert_eq!(tree.dirty_leaves(), 1);
    assert_eq!(tree.remove(&3u32.to_be_bytes()), Ok(false));
    tree.flush().unwrap();
    assert_eq!(stored_pages(&tree), resident_pages(&tree));
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn memory_and_file_backends_store_identical_bytes() {
    let config = config(256, 128, 8, 16);
    let dir = tempfile::tempdir().unwrap();
    let file =
        FileStore::create(dir.path().join("tree.bf"), Superblock::from_config(&config)).unwrap();
    let mem = MemStore::new(Superblock::from_config(&config)).unwrap();
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
        assert_eq!(tree.dirty_leaves(), 0);
        assert_eq!(tree.check_invariants(), Ok(()));
    }

    let [mem, file] = trees;
    assert_eq!(stored_pages(&mem), stored_pages(&file));
    assert_eq!(stored_pages(&file), resident_pages(&file));
    assert_eq!(collect(mem.iter()), collect(file.iter()));
    assert!(mem.height() >= 2);
}
