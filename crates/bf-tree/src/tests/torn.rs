//! Torn and suspect pages on open: every row of plan 0018's selection
//! table, at every 64-byte tear (plan 0018, diff 5).

use super::*;

const PAGE: usize = 256;
const SECTOR: usize = 64;
const SECTORS: usize = PAGE / SECTOR;

fn small() -> TreeConfig {
    config(PAGE, 128, 8, 16)
}

fn key(i: u32) -> [u8; 4] {
    i.to_be_bytes()
}

/// A committed multi-leaf tree, and one key in leaf 0 with its committed
/// value.
fn committed() -> (Tree, [u8; 4]) {
    let tree = Tree::in_memory(small()).unwrap();
    for i in 0..200 {
        tree.put(&key(i), b"old").unwrap();
    }
    tree.checkpoint().unwrap();
    assert!(tree.leaf_count() > 2);
    let probe = (0..200)
        .map(key)
        .find(|k| tree.find_leaf(k) == LeafId::new(0).unwrap())
        .unwrap();
    (tree, probe)
}

/// Write leaf 0 through once more after the commit, into a fresh page
/// number (`fresh`) or a freed one that holds an older page. Returns the
/// page number, its bytes before the write, and the new page's bytes.
fn late_write(tree: &Tree, probe: &[u8], fresh: bool) -> (PageNumber, Vec<u8>, Vec<u8>) {
    let store = tree.store();
    if fresh {
        // Claim every freed page, so the next allocation grows the store.
        let count = store.page_count();
        while store.alloc().unwrap().raw() < count {}
    }
    // Allocation is last in, first out: peek at the page the write gets.
    let next = store.alloc().unwrap();
    let mut before = vec![0; PAGE];
    store.read(next, &mut before).unwrap();
    store.free(next).unwrap();
    if !fresh {
        assert!(before.iter().any(|&byte| byte != 0), "an older page");
    }

    tree.put(probe, b"new").unwrap();
    let id = LeafId::new(0).unwrap();
    let mut bytes = tree.leaves.page_copy(id).unwrap();
    tree.leaves.overwrite(id, &mut bytes).unwrap();
    assert_eq!(tree.leaves.page_of(id), Some(next));
    let mut after = vec![0; PAGE];
    store.read(next, &mut after).unwrap();
    (next, before, after)
}

/// The page as a crash leaves it: the sectors in `landed` hold the new
/// write, the others the old bytes.
fn torn(before: &[u8], after: &[u8], landed: u32) -> Vec<u8> {
    (0..SECTORS)
        .flat_map(|sector| {
            let source = if landed & (1 << sector) != 0 {
                after
            } else {
                before
            };
            source[sector * SECTOR..(sector + 1) * SECTOR].to_vec()
        })
        .collect()
}

#[test]
fn every_tear_of_a_late_write_reopens_at_the_commit() {
    for fresh in [true, false] {
        for landed in 0..(1u32 << SECTORS) {
            let (tree, probe) = committed();
            let (page_number, before, after) = late_write(&tree, &probe, fresh);
            tree.store()
                .write(page_number, &torn(&before, &after, landed))
                .unwrap();
            let context = format!("fresh {fresh}, landed sectors {landed:04b}");
            let tree = Tree::open(tree.into_store()).expect(&context);
            assert_eq!(tree.get(&probe), Ok(Some(b"old".to_vec())), "{context}");
            assert_eq!(tree.check_invariants(), Ok(()), "{context}");
        }
    }
}

#[test]
fn damage_to_a_committed_page_is_reported() {
    let (tree, _) = committed();
    let id = LeafId::new(1).unwrap();
    let page_number = tree.leaves.page_of(id).unwrap();
    let mut bytes = vec![0; PAGE];
    tree.store().read(page_number, &mut bytes).unwrap();
    // A flipped body byte: the header still claims a committed page.
    bytes[200] ^= 0x01;
    tree.store().write(page_number, &bytes).unwrap();
    assert_eq!(
        Tree::open(tree.into_store()).err(),
        Some(TreeError::CorruptPage {
            page_number,
            reason: "checksum mismatch",
        })
    );
}

#[test]
fn a_sealed_page_that_fails_validation_is_reported() {
    let (tree, _) = committed();
    let id = LeafId::new(1).unwrap();
    let page_number = tree.leaves.page_of(id).unwrap();
    let mut bytes = vec![0; PAGE];
    tree.store().read(page_number, &mut bytes).unwrap();
    bytes[3] ^= 0x40; // the capacity field
    let page: &mut Page = zerocopy::FromBytes::mut_from_bytes(&mut bytes[..]).unwrap();
    page.seal();
    tree.store().write(page_number, &bytes).unwrap();
    assert!(matches!(
        Tree::open(tree.into_store()).err(),
        Some(TreeError::CorruptPage { page_number: p, .. }) if p == page_number
    ));
}
