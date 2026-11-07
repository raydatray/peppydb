//! Shadow paging: every write-through goes to a fresh page (plan 0018).

use super::*;

fn leaf(raw: u64) -> LeafId {
    LeafId::new(raw).unwrap()
}

/// The stored page of leaf `id` and its write sequence.
fn stored(tree: &Tree, id: LeafId) -> (PageNumber, Page_) {
    let page_number = tree.leaves.page_of(id).unwrap();
    let mut bytes = vec![0; tree.store().page_size()];
    tree.store().read(page_number, &mut bytes).unwrap();
    (page_number, Page_(bytes))
}

struct Page_(Vec<u8>);

impl Page_ {
    fn write_seq(&self) -> u64 {
        Page::validate(&self.0).unwrap().write_seq()
    }
}

#[test]
fn a_write_through_moves_the_leaf_and_supersedes_its_old_page() {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    let (first, page) = stored(&tree, leaf(0));
    assert_eq!(page.write_seq(), 1, "create wrote sequence 1");
    tree.put(b"a", b"1").unwrap();
    tree.flush().unwrap();
    let (second, page) = stored(&tree, leaf(0));
    assert_ne!(second, first, "a fresh page");
    assert_eq!(page.write_seq(), 2);
    assert_eq!(tree.leaves.write_seq(), 2);
    // Flush freed the replaced page: the next write reuses it.
    tree.put(b"b", b"2").unwrap();
    tree.flush().unwrap();
    assert_eq!(stored(&tree, leaf(0)).0, first);
    assert_eq!(tree.store().page_count(), 2);
}

#[test]
fn supersessions_wait_for_a_flush() {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    let mut bytes = tree.leaves.page_copy(leaf(0)).unwrap();
    tree.leaves.overwrite(leaf(0), &mut bytes).unwrap();
    tree.leaves.overwrite(leaf(0), &mut bytes).unwrap();
    let superseded = tree.leaves.take_superseded();
    assert_eq!(superseded.len(), 2, "both earlier pages wait");
    assert_eq!(
        superseded.iter().map(|&(_, seq)| seq).collect::<Vec<_>>(),
        [2, 3]
    );
    assert_eq!(tree.store().page_count(), 3, "nothing was overwritten");
}

#[test]
fn split_halves_share_one_write_sequence() {
    let tree = Tree::in_memory(config(256, 128, 8, 16)).unwrap();
    let mut i = 0u32;
    while tree.leaf_count() == 1 {
        tree.put(&i.to_be_bytes(), &[1; 16]).unwrap();
        i += 1;
    }
    // Before any later write-through of either half.
    let (_, left) = stored(&tree, leaf(0));
    let (_, right) = stored(&tree, leaf(1));
    assert_eq!(left.write_seq(), right.write_seq());
}

#[test]
fn after_flush_and_reopen_each_leaf_has_one_page_and_sequences_continue() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shadow.bf");
    let config = config(256, 128, 8, 16);
    let store = FileStore::create(&path, Superblock::from(&config)).unwrap();
    let tree = Tree::create(config, Box::new(store)).unwrap();
    for i in 0..500u32 {
        tree.put(&i.wrapping_mul(2_654_435_761).to_be_bytes(), &[i as u8; 12])
            .unwrap();
        if i % 97 == 0 {
            tree.flush().unwrap();
        }
    }
    tree.flush().unwrap();
    let max = tree.leaves.write_seq();
    let live = tree.leaves.page_table().into_iter().flatten().count();
    drop(tree);

    let tree = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    assert_eq!(tree.leaves.page_table().into_iter().flatten().count(), live);
    assert_eq!(
        tree.leaves.write_seq(),
        max,
        "the counter restarts above every page"
    );
    tree.put(b"zz", b"v").unwrap();
    tree.flush().unwrap();
    assert!(tree.leaves.write_seq() > max);
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn open_keeps_the_newest_copy_of_a_leaf_and_releases_older_ones() {
    let config = config(256, 128, 8, 16);
    let tree = Tree::in_memory(config).unwrap();
    tree.put(b"a", b"old").unwrap();
    tree.flush().unwrap();
    let (old_page, _) = stored(&tree, leaf(0));
    let mut old_bytes = vec![0; 256];
    tree.store().read(old_page, &mut old_bytes).unwrap();
    tree.put(b"a", b"new").unwrap();
    tree.flush().unwrap();
    // Plant the older copy back into a freed page, as a crash would leave it.
    let freed = tree.store().alloc().unwrap();
    tree.store().write(freed, &old_bytes).unwrap();
    let tree = Tree::open(tree.into_store()).unwrap();
    assert_eq!(tree.get(b"a"), Ok(Some(b"new".to_vec())));
    assert_eq!(
        tree.store().alloc().unwrap(),
        freed,
        "the older copy was released"
    );
}
