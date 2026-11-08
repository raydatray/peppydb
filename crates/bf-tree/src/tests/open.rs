//! The tiling check and the bulk build of inner levels, as pure functions.

use super::*;
use crate::open::{LeafBounds, build_inner, tile};
use bf_tree_store::FileStore;

fn leaf(raw: u64) -> LeafId {
    LeafId::new(raw).unwrap()
}

fn bounds(id: u64, lower: Option<&[u8]>, upper: Option<&[u8]>) -> LeafBounds {
    LeafBounds {
        id: leaf(id),
        lower: lower.map(<[u8]>::to_vec),
        upper: upper.map(<[u8]>::to_vec),
    }
}

/// `n` leaves over keys `0..n` encoded big-endian, in shuffled ID order.
fn tiling(n: u32) -> Vec<LeafBounds> {
    let key = |i: u32| i.to_be_bytes().to_vec();
    let mut leaves: Vec<_> = (0..n)
        .map(|i| LeafBounds {
            id: leaf(u64::from((i * 7 + 3) % n)),
            lower: (i > 0).then(|| key(i)),
            upper: (i + 1 < n).then(|| key(i + 1)),
        })
        .collect();
    leaves.rotate_left((n / 3) as usize);
    leaves
}

#[test]
fn tile_sorts_a_valid_tiling_by_lower_fence() {
    for n in [1, 2, 3, 10, 257] {
        let sorted = tile(tiling(n)).unwrap();
        assert_eq!(sorted.len(), n as usize);
        assert_eq!(sorted[0].lower, None);
        assert_eq!(sorted[n as usize - 1].upper, None);
        for pair in sorted.windows(2) {
            assert_eq!(pair[0].upper, pair[1].lower);
            assert!(pair[1].lower.is_some());
        }
    }
}

#[test]
fn tile_reports_every_inconsistency() {
    let reason = |leaves: Vec<LeafBounds>| match tile(leaves) {
        Err(TreeError::InconsistentLeaves(reason)) => reason,
        other => panic!("expected inconsistent leaves, got {other:?}"),
    };

    assert_eq!(reason(vec![]), "store holds no leaves");
    assert_eq!(
        reason(vec![bounds(0, Some(b"a"), None)]),
        "no leaf has an unbounded lower fence"
    );
    assert_eq!(
        reason(vec![bounds(0, None, Some(b"m")), bounds(1, None, None)]),
        "two leaves have unbounded lower fences"
    );
    assert_eq!(
        reason(vec![bounds(0, None, None), bounds(1, Some(b"m"), None)]),
        "a leaf before the last has an unbounded upper fence"
    );
    // Gap: [-inf, m) then [n, +inf).
    assert_eq!(
        reason(vec![
            bounds(0, None, Some(b"m")),
            bounds(1, Some(b"n"), None)
        ]),
        "adjacent leaf fences leave a gap or overlap"
    );
    // Overlap: [-inf, n) then [m, +inf).
    assert_eq!(
        reason(vec![
            bounds(0, None, Some(b"n")),
            bounds(1, Some(b"m"), None)
        ]),
        "adjacent leaf fences leave a gap or overlap"
    );
    // Duplicate leaf: [m, t) twice.
    assert_eq!(
        reason(vec![
            bounds(0, None, Some(b"m")),
            bounds(1, Some(b"m"), Some(b"t")),
            bounds(2, Some(b"m"), Some(b"t")),
            bounds(3, Some(b"t"), None),
        ]),
        "adjacent leaf fences leave a gap or overlap"
    );
    assert_eq!(
        reason(vec![bounds(0, None, Some(b"m"))]),
        "the last leaf has a bounded upper fence"
    );
    // A reversed middle leaf whose neighbours happen to line up.
    assert_eq!(
        reason(vec![
            bounds(0, None, Some(b"t")),
            bounds(1, Some(b"t"), Some(b"m")),
            bounds(2, Some(b"m"), None),
        ]),
        "adjacent leaf fences leave a gap or overlap"
    );
}

/// Build a tree over `n` tiling leaves whose pages exist, so that the
/// invariant checker can compare fences with the routing intervals.
fn built(n: u32, inner_capacity: usize) -> Tree {
    let config = config(256, inner_capacity, 8, 16);
    let tree = Tree::assemble(
        config,
        Inner::new(config.inner_capacity, ChildRef::Leaf(leaf(0))),
        LeafMap::for_config(
            Box::new(MemStore::new(Superblock::from(&config)).unwrap()),
            [],
            &config,
        )
        .unwrap(),
    );
    let leaves = tile(tiling(n)).unwrap();
    for _ in 0..n {
        tree.leaves.alloc_id().unwrap();
    }
    for bounds in &leaves {
        let fences = Fences {
            lower: bounds.lower.as_deref().map_or(Fence::Unbounded, Fence::Key),
            upper: bounds.upper.as_deref().map_or(Fence::Unbounded, Fence::Key),
        };
        let mut bytes = vec![0; config.leaf_capacity];
        let page = Page::init_with_fences(&mut bytes, fences).unwrap();
        if let Some(lower) = &bounds.lower {
            page.try_insert(lower, b"v").unwrap();
        }
        tree.leaves.overwrite(bounds.id, &mut bytes).unwrap();
    }
    tree.inner
        .set_root(build_inner(&tree.inner, &leaves).unwrap());
    tree
}

#[test]
fn single_leaf_builds_a_leaf_root() {
    let tree = built(1, 64);
    assert_eq!(tree.root(), ChildRef::Leaf(leaf(0)));
    assert_eq!(tree.inner_count(), 0);
    assert_eq!(tree.height(), 1);
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn builds_one_two_and_three_inner_levels() {
    // 64-byte nodes hold (64 - 16) / 16 = 3 four-byte separators, so four
    // children: heights step at 4, 16, and 64 leaves.
    let cases = [(2, 2), (4, 2), (5, 3), (16, 3), (17, 4), (64, 4), (65, 5)];
    for (n, expected_height) in cases {
        let tree = built(n, 64);
        assert_eq!(tree.height(), expected_height, "{n} leaves");
        assert_eq!(tree.check_invariants(), Ok(()), "{n} leaves");
    }

    let tree = built(1000, 4096);
    assert_eq!(tree.height(), 3);
    assert_eq!(tree.check_invariants(), Ok(()));
}

#[test]
fn routing_from_the_built_tree_matches_a_linear_fence_search() {
    for (n, inner_capacity) in [(5, 64), (40, 64), (300, 128), (1000, 4096)] {
        let tree = built(n, inner_capacity);
        let leaves = tile(tiling(n)).unwrap();
        let expected = |key: &[u8]| {
            leaves
                .iter()
                .rev()
                .find(|leaf| leaf.lower.as_deref().is_none_or(|lower| key >= lower))
                .unwrap()
                .id
        };
        for i in 0..n * 2 + 3 {
            for key in [i.to_be_bytes().to_vec(), [i.to_be_bytes(), [1; 4]].concat()] {
                assert_eq!(
                    tree.find_leaf(&key),
                    expected(&key),
                    "{n} leaves, key {key:?}"
                );
            }
        }
        assert_eq!(tree.find_leaf(b""), leaves[0].id);
        assert_eq!(tree.find_leaf(&[0xff; 8]), leaves.last().unwrap().id);
    }
}

#[test]
fn built_trees_accept_further_writes_and_splits() {
    let tree = built(20, 64);
    let height = tree.height();
    for i in 0..200u32 {
        tree.put(&(i * 11).to_be_bytes(), &[2; 16]).unwrap();
    }
    assert!(tree.height() >= height);
    assert_eq!(tree.check_invariants(), Ok(()));
    assert_eq!(tree.get(&(11 * 7u32).to_be_bytes()), Ok(Some(vec![2; 16])));
}

#[test]
fn a_trailing_single_child_borrows_from_its_neighbour() {
    // Four children per node: 5 leaves would leave the last node with one
    // child, so the split becomes 3 + 2.
    let tree = built(5, 64);
    let ChildRef::Inner(root) = tree.inner.root() else {
        panic!("root is inner");
    };
    let root = tree.inner.snapshot(root);
    let children: Vec<_> = (0..root.node().child_count())
        .map(|index| match root.node().child_at(index).unwrap() {
            ChildRef::Inner(id) => tree.inner.snapshot(id).node().child_count(),
            ChildRef::Leaf(_) => panic!("leaves sit below the second level"),
        })
        .collect();
    assert_eq!(children, [3, 2]);
}

// --- Tree::open ---------------------------------------------------------

type Model = std::collections::BTreeMap<Vec<u8>, Vec<u8>>;

fn fill(tree: &mut Tree, model: &mut Model, count: u32) {
    for i in 0..count {
        let key = (i.wrapping_mul(2_654_435_761) % 100_000).to_be_bytes();
        let value = vec![i as u8; (i % 17) as usize];
        tree.put(&key, &value).unwrap();
        model.insert(key.to_vec(), value);
        if i % 5 == 0 {
            let victim = ((i / 2).wrapping_mul(2_654_435_761) % 100_000).to_be_bytes();
            tree.remove(&victim).unwrap();
            model.remove(victim.as_slice());
        }
    }
}

fn assert_matches_model(tree: &Tree, model: &Model) {
    assert_eq!(tree.check_invariants(), Ok(()));
    for (key, value) in model {
        assert_eq!(tree.get(key), Ok(Some(value.clone())), "{key:?}");
    }
    let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(collect(tree.iter()), expected);
}

#[test]
fn reopened_memory_trees_match_the_oracle_at_every_height() {
    let config = config(256, 128, 8, 16);
    for count in [0, 3, 60, 400, 3000] {
        let store = MemStore::new(Superblock::from(&config)).unwrap();
        let mut tree = Tree::create(config, Box::new(store)).unwrap();
        let mut model = Model::new();
        fill(&mut tree, &mut model, count);
        tree.checkpoint().unwrap();
        let height = tree.height();
        let leaves = tree.leaf_count();

        let reopened = Tree::open(tree.into_store()).unwrap();
        // The layout comes from the superblock; runtime settings default.
        assert_eq!(
            Superblock::from(reopened.config()),
            Superblock::from(&config)
        );
        // Bulk-built levels are packed, so the rebuilt tree is never taller.
        assert!(reopened.height() <= height, "{count} records");
        assert!(reopened.height() >= height.min(3), "{count} records");
        assert_eq!(reopened.leaf_count(), leaves);
        assert_matches_model(&reopened, &model);
    }
}

#[test]
fn reopened_file_trees_survive_drop_and_keep_accepting_writes() {
    let config = config(512, 256, 16, 64);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.bf");
    let mut model = Model::new();
    let height = {
        let store = FileStore::create(&path, Superblock::from(&config)).unwrap();
        let mut tree = Tree::create(config, Box::new(store)).unwrap();
        fill(&mut tree, &mut model, 2000);
        tree.checkpoint().unwrap();
        tree.height()
    };
    assert!(height >= 3);

    let mut tree = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    assert!((3..=height).contains(&tree.height()));
    assert_matches_model(&tree, &model);

    fill(&mut tree, &mut model, 500);
    assert_matches_model(&tree, &model);
    tree.checkpoint().unwrap();
    drop(tree);

    let tree = Tree::open(Box::new(FileStore::open(&path).unwrap())).unwrap();
    assert_matches_model(&tree, &model);
}

#[test]
fn a_closed_trees_store_holds_every_flushed_write_and_no_buffered_one() {
    let config = config(256, 128, 8, 16);
    let store = MemStore::new(Superblock::from(&config)).unwrap();
    let mut tree = Tree::create(config, Box::new(store)).unwrap();
    let mut model = Model::new();
    fill(&mut tree, &mut model, 100);
    tree.checkpoint().unwrap();
    // A second batch, including splits, then a flush.
    let leaves_before = tree.leaf_count();
    fill(&mut tree, &mut model, 300);
    tree.checkpoint().unwrap();
    let leaves_after = tree.leaf_count();
    assert!(leaves_after > leaves_before);
    // One more put after the flush. It is buffered in a fresh mini-page, so
    // it cannot cause a merge, and `into_store` loses it.
    let lost = 999_999u32.to_be_bytes();
    tree.put(&lost, b"lost").unwrap();
    assert_eq!(tree.buffered_leaves(), 1);
    assert_eq!(tree.leaf_count(), leaves_after);

    let mut reopened = Tree::open(tree.into_store()).unwrap();
    // The store holds both flushed batches and not the buffered put.
    assert_matches_model(&reopened, &model);
    assert_eq!(reopened.get(&lost), Ok(None));
    // Every allocated slot was written, so none reopens as free.
    assert_eq!(reopened.leaf_count(), leaves_after);
    assert!((0..leaves_after).all(|index| reopened.leaves.is_live(leaf(index as u64))));
    fill(&mut reopened, &mut model, 300);
    assert_matches_model(&reopened, &model);
}

/// A flushed store of a small multi-leaf tree, the leaf images, and each
/// leaf's page number, for corruption fixtures.
fn flushed_store() -> (Box<dyn PageStore>, Vec<Vec<u8>>, Vec<PageNumber>) {
    let config = config(256, 128, 8, 16);
    let store = MemStore::new(Superblock::from(&config)).unwrap();
    let mut tree = Tree::create(config, Box::new(store)).unwrap();
    let mut model = Model::new();
    fill(&mut tree, &mut model, 200);
    tree.checkpoint().unwrap();
    assert!(tree.leaf_count() >= 4);
    let images = (0..tree.leaf_count())
        .map(|index| tree.leaves.page_copy(leaf(index as u64)).unwrap())
        .collect();
    let pages = tree.leaves.page_table().into_iter().flatten().collect();
    (tree.into_store(), images, pages)
}

#[test]
fn open_rejects_damaged_leaf_sets_with_errors_not_panics() {
    // A middle leaf with every stored copy zeroed leaves a gap. Freed
    // pages keep older copies, so all of them are zeroed.
    let (store, _, _) = flushed_store();
    // Claim every freed page, so that each can be written.
    let count = store.page_count();
    while store.alloc().unwrap().raw() < count {}
    let mut bytes = vec![0; 256];
    for raw in 0..count {
        let page_number = PageNumber::new(raw).unwrap();
        store.read(page_number, &mut bytes).unwrap();
        let names_leaf_1 = bf_tree_page::raw_header_stamps(&bytes)
            .is_some_and(|(seq, id)| seq != 0 && id == Some(leaf(1)));
        if names_leaf_1 {
            store.write(page_number, &[0; 256]).unwrap();
        }
    }
    assert_eq!(
        Tree::open(store).err(),
        Some(TreeError::InconsistentLeaves(
            "adjacent leaf fences leave a gap or overlap"
        ))
    );

    // A duplicated leaf: leaf 1's page written over leaf 2's.
    let (store, images, pages) = flushed_store();
    store.write(pages[2], &images[1]).unwrap();
    assert_eq!(
        Tree::open(store).err(),
        Some(TreeError::InconsistentLeaves(
            "two stored pages name the same leaf"
        ))
    );

    // A torn page: the tail of a leaf zeroed.
    let (store, images, pages) = flushed_store();
    let mut torn = images[1].clone();
    torn[128..].fill(0);
    store.write(pages[1], &torn).unwrap();
    assert_eq!(
        Tree::open(store).err(),
        Some(TreeError::CorruptPage {
            page_number: pages[1],
            reason: "checksum mismatch",
        })
    );

    // A flipped header byte fails the checksum; resealed, it fails
    // validation.
    let (store, images, pages) = flushed_store();
    let mut flipped = images[0].clone();
    flipped[3] ^= 0x40;
    store.write(pages[0], &flipped).unwrap();
    assert_eq!(
        Tree::open(store).err(),
        Some(TreeError::CorruptPage {
            page_number: pages[0],
            reason: "checksum mismatch",
        })
    );
    let (store, images, pages) = flushed_store();
    let mut flipped = images[0].clone();
    flipped[3] ^= 0x40;
    stamp(&mut flipped, 0);
    store.write(pages[0], &flipped).unwrap();
    assert_eq!(
        Tree::open(store).err(),
        Some(TreeError::CorruptPage {
            page_number: pages[0],
            reason: "capacity field differs from the buffer length",
        })
    );

    // A valid but unfenced page.
    let (store, _, pages) = flushed_store();
    let mut unfenced = vec![0; 256];
    Page::init(&mut unfenced).unwrap();
    stamp(&mut unfenced, 0);
    store.write(pages[0], &unfenced).unwrap();
    assert_eq!(
        Tree::open(store).err(),
        Some(TreeError::InconsistentLeaves("leaf has no fences"))
    );

    // An empty store, and a store whose only leaf is freed.
    let config = config(256, 128, 8, 16);
    let empty = MemStore::new(Superblock::from(&config)).unwrap();
    assert_eq!(
        Tree::open(Box::new(empty)).err(),
        Some(TreeError::InconsistentLeaves("store holds no leaves"))
    );
}

#[test]
fn open_rejects_configurations_the_pages_do_not_respect() {
    // A superblock whose admission rule cannot hold.
    let bad = Superblock {
        page_size: 64,
        inner_capacity: 64,
        max_key_len: 1,
        max_value_len: 0,
    };
    let store = MemStore::new(bad).unwrap();
    assert!(matches!(
        Tree::open(Box::new(store)).err(),
        Some(TreeError::InvalidConfig(_))
    ));

    // Pages written under a wider key limit than the superblock claims.
    let narrow = config(256, 128, 4, 16);
    let store = MemStore::new(Superblock::from(&narrow)).unwrap();
    let id = store.alloc().unwrap();
    let mut bytes = vec![0; 256];
    let page = Page::init_with_fences(
        &mut bytes,
        Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        },
    )
    .unwrap();
    page.try_insert(&[1; 8], b"v").unwrap();
    stamp(&mut bytes, 0);
    store.write(id, &bytes).unwrap();
    assert_eq!(
        Tree::open(Box::new(store)).err(),
        Some(TreeError::InconsistentLeaves(
            "record exceeds the configured key or value length"
        ))
    );

    let store = MemStore::new(Superblock::from(&narrow)).unwrap();
    for (lower, upper) in [
        (None, Some([7u8; 8].as_slice())),
        (Some([7u8; 8].as_slice()), None),
    ] {
        let id = store.alloc().unwrap();
        let mut bytes = vec![0; 256];
        Page::init_with_fences(
            &mut bytes,
            Fences {
                lower: lower.map_or(Fence::Unbounded, Fence::Key),
                upper: upper.map_or(Fence::Unbounded, Fence::Key),
            },
        )
        .unwrap();
        stamp(&mut bytes, id.raw());
        store.write(id, &bytes).unwrap();
    }
    assert_eq!(
        Tree::open(Box::new(store)).err(),
        Some(TreeError::InconsistentLeaves(
            "fence exceeds the configured key length"
        ))
    );
}

/// A tree over a direct-I/O store (plan 0023): writes, checkpoints, an
/// uncommitted tail, and reopens in both modes. Linux only; skipped where
/// the file system rejects `O_DIRECT`.
#[cfg(target_os = "linux")]
#[test]
fn a_tree_over_a_direct_store_reopens_at_its_commit() {
    use bf_tree_store::Io;
    let dir = match std::env::var_os("PEPPYDB_BENCH_DIR") {
        Some(base) => tempfile::tempdir_in(base).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let path = dir.path().join("direct-tree.bf");
    let config = TreeConfig {
        circular_buffer_bytes: 256 << 10,
        ..TreeConfig::default()
    };
    let store = match FileStore::create_with(&path, Superblock::from(&config), Io::Direct) {
        Ok(store) => store,
        Err(StoreError::Io { kind, .. }) if kind == std::io::ErrorKind::InvalidInput => {
            eprintln!("skipped: {} rejects O_DIRECT", dir.path().display());
            return;
        }
        Err(error) => panic!("{error:?}"),
    };
    let tree = Tree::create(config, Box::new(store)).unwrap();
    let mut model = std::collections::BTreeMap::new();
    for i in 0..20_000u32 {
        let key = i.wrapping_mul(2_654_435_761).to_be_bytes();
        tree.put(&key, &[i as u8; 40]).unwrap();
        model.insert(key.to_vec(), vec![i as u8; 40]);
        if i % 7_000 == 6_999 {
            tree.checkpoint().unwrap();
        }
    }
    tree.checkpoint().unwrap();
    // Uncommitted: lost on reopen.
    for i in 0..500u32 {
        tree.put(&(1_000_000 + i).to_be_bytes(), b"lost").unwrap();
    }
    drop(tree);
    for io in [Io::Direct, Io::Cached] {
        let store = FileStore::open_with(&path, io).unwrap();
        let tree = Tree::open_with(config, Box::new(store)).unwrap();
        let all: std::collections::BTreeMap<_, _> = tree.iter().map(Result::unwrap).collect();
        assert_eq!(all, model, "{io:?}");
        assert_eq!(tree.check_invariants(), Ok(()));
    }
}

/// Open reads pages in runs; the selection does not depend on the run
/// length, including a page count that is not a multiple of it.
#[test]
fn the_open_scan_selects_the_same_pages_with_any_run_length() {
    use crate::open::select_in_runs;
    let config = config(256, 128, 8, 16);
    let tree = Tree::in_memory(config).unwrap();
    for i in 0..3000u32 {
        tree.put(&i.wrapping_mul(2_654_435_761).to_be_bytes(), &[i as u8; 12])
            .unwrap();
        if i % 900 == 0 {
            tree.checkpoint().unwrap();
        }
    }
    tree.checkpoint().unwrap();
    // Uncommitted pages, to be erased.
    for i in 0..300u32 {
        tree.put(&i.to_be_bytes(), b"late").unwrap();
    }
    let written = tree.committed().write_seq;
    let store = tree.store();
    let page_count = store.page_count();
    assert!(
        !page_count.is_multiple_of(64) && !page_count.is_multiple_of(7),
        "{page_count}"
    );
    let summary = |run| {
        let selection = select_in_runs(store, &config, written, run).unwrap();
        let mut chosen: Vec<_> = selection
            .chosen
            .iter()
            .map(|(id, chosen)| (id.raw(), chosen.page_number))
            .collect();
        chosen.sort_unstable();
        let mut others = [selection.released, selection.zero, selection.erased];
        others.iter_mut().for_each(|pages| pages.sort_unstable());
        (chosen, others, selection.max_seen)
    };
    let one = summary(1);
    assert!(!one.0.is_empty());
    assert_eq!(summary(7), one);
    assert_eq!(summary(64), one);
    assert_eq!(summary(page_count + 5), one);
}
