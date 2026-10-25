//! Reopening a tree from its stored leaves.
//!
//! Inner nodes are derived data. `Tree::open` reads and validates every slot,
//! then `tile` checks that the leaf fences partition the key space and
//! `build_inner` packs the separators into inner levels bottom-up. The last
//! two are pure functions over fence lists, so they are tested without a
//! store.

use std::cmp::Ordering;
use std::ops::Range;

use super::leaf_map::LeafMap;
use super::store::InnerStore;
use super::{Tree, TreeError};
use crate::inner::{HEADER_BYTES, InnerError, InnerNode, entry_size};
use crate::store::{PageStore, StoreError};
use crate::{ChildRef, Fence, KeyRef, LeafId, Page, PageError};

impl Tree {
    /// Open a tree from a store written by [`Tree::create`] and
    /// [`Tree::flush`], rebuilding the routing layer from leaf fences.
    ///
    /// The configuration comes from the store's superblock. Every slot is
    /// read once: all-zero slots are free (allocated but never flushed) and
    /// are returned to the store for reuse; every other slot must pass
    /// [`Page::validate`], carry fences, and respect the configured key and
    /// value limits. The leaves must tile the key space, so a store with a
    /// missing, duplicated, or torn leaf is rejected rather than opened.
    /// Open is `O(leaves)` in I/O and memory; no leaf stays resident.
    pub fn open(mut store: Box<dyn PageStore>) -> Result<Self, TreeError> {
        let config = store.superblock().to_config();
        config.validate()?;

        let mut freed = Vec::new();
        let mut leaves = Vec::new();
        let mut bytes = vec![0; store.page_size()];
        for raw in 0..store.slots() {
            let id = LeafId::new(raw).expect("slot counts stay below the reserved ID");
            store.read(id, &mut bytes)?;
            let is_free = bytes.iter().all(|&byte| byte == 0);
            freed.push(is_free);
            if is_free {
                // A slot allocated but never flushed. Return it to the store's
                // free list; a backend that rescanned on open already has it.
                match store.free(id) {
                    Ok(()) | Err(StoreError::FreedSlot(_)) => continue,
                    Err(error) => return Err(error.into()),
                }
            }

            let page = Page::validate(&bytes).map_err(|error| match error {
                PageError::Corrupt(reason) => TreeError::CorruptLeaf { id, reason },
                other => TreeError::Page(other),
            })?;
            let fences = page
                .fences()
                .ok_or(TreeError::InconsistentLeaves("leaf has no fences"))?;
            let bound = |fence: Fence<KeyRef<'_>>| match fence {
                Fence::Unbounded => Ok(None),
                Fence::Key(key) if key.len() <= config.max_key_len => Ok(Some(key.to_vec())),
                Fence::Key(_) => Err(TreeError::InconsistentLeaves(
                    "fence exceeds the configured key length",
                )),
            };
            let within_limits = page.iter().all(|record| {
                record.key.len() <= config.max_key_len
                    && record
                        .state
                        .value()
                        .is_none_or(|value| value.len() <= config.max_value_len)
            });
            if !within_limits {
                return Err(TreeError::InconsistentLeaves(
                    "record exceeds the configured key or value length",
                ));
            }

            leaves.push(LeafBounds {
                id,
                lower: bound(fences.lower)?,
                upper: bound(fences.upper)?,
            });
        }

        let leaves = tile(leaves)?;
        let mut inner = InnerStore::new(config.inner_capacity);
        let root = build_inner(&mut inner, &leaves)?;

        Ok(Self {
            config,
            root,
            inner,
            leaves: LeafMap::new(store, freed),
        })
    }
}

/// One leaf's identity and bounds; `None` is an unbounded fence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LeafBounds {
    pub(super) id: LeafId,
    pub(super) lower: Option<Vec<u8>>,
    pub(super) upper: Option<Vec<u8>>,
}

/// Sort leaves by lower fence and check that they tile the key space:
/// exactly one unbounded lower fence, exactly one unbounded upper fence, and
/// `upper(i) == lower(i + 1)` for every adjacent pair.
pub(super) fn tile(mut leaves: Vec<LeafBounds>) -> Result<Vec<LeafBounds>, TreeError> {
    let inconsistent = TreeError::InconsistentLeaves;
    if leaves.is_empty() {
        return Err(inconsistent("store holds no leaves"));
    }

    // An unbounded lower fence sorts first.
    leaves.sort_by(|a, b| match (&a.lower, &b.lower) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(a), Some(b)) => a.cmp(b),
    });

    if leaves[0].lower.is_some() {
        return Err(inconsistent("no leaf has an unbounded lower fence"));
    }

    for pair in leaves.windows(2) {
        let (left, right) = (&pair[0], &pair[1]);
        if right.lower.is_none() {
            return Err(inconsistent("two leaves have unbounded lower fences"));
        }
        if left.upper.is_none() {
            return Err(inconsistent(
                "a leaf before the last has an unbounded upper fence",
            ));
        }
        if left.upper != right.lower {
            return Err(inconsistent("adjacent leaf fences leave a gap or overlap"));
        }
        if let (Some(lower), Some(upper)) = (&left.lower, &left.upper)
            && lower >= upper
        {
            return Err(inconsistent("leaf fences are not ordered"));
        }
    }

    if leaves.last().expect("nonempty").upper.is_some() {
        return Err(inconsistent("the last leaf has a bounded upper fence"));
    }

    Ok(leaves)
}

/// Pack sorted, tiling leaves into inner levels and return the root.
///
/// Each level is filled left to right by entry size, exactly as
/// `InnerNode::try_insert` would accept separators until `NodeFull`. The
/// first child of every node after the first has its separator promoted to
/// the level above. A single leaf is its own root. Full nodes are correct:
/// the next insert into one simply splits it.
///
/// A trailing node would sometimes get one child and no separator, which
/// the tree invariants forbid; it borrows the last entry of its left
/// neighbour instead. Under the admission rule a full node holds at least
/// two separators, so the neighbour keeps at least one.
pub(super) fn build_inner(
    inner: &mut InnerStore,
    leaves: &[LeafBounds],
) -> Result<ChildRef, TreeError> {
    // Each entry is a child and the separator to its left; the first entry
    // of a level has none.
    let mut level: Vec<Entry> = leaves
        .iter()
        .map(|leaf| (leaf.lower.clone(), ChildRef::Leaf(leaf.id)))
        .collect();

    while level.len() > 1 {
        let groups = group(&level, inner.capacity());
        if groups.len() == level.len() {
            return Err(InnerError::SeparatorTooLarge.into());
        }

        let mut above = Vec::with_capacity(groups.len());
        for range in groups {
            let entries = &level[range];
            let (separator, first) = &entries[0];
            let id = inner.alloc();
            let node = InnerNode::init(inner.get_mut(id), *first)?;
            for (separator, child) in &entries[1..] {
                let key = separator
                    .as_deref()
                    .expect("every child after the first has a separator");
                node.try_insert(key, *child)?;
            }
            above.push((separator.clone(), ChildRef::Inner(id)));
        }

        level = above;
    }

    let (_, root) = level.into_iter().next().expect("one root remains");

    Ok(root)
}

type Entry = (Option<Vec<u8>>, ChildRef);

/// Split one level into runs of entries that fit one node each.
fn group(level: &[Entry], capacity: usize) -> Vec<Range<usize>> {
    let budget = capacity - HEADER_BYTES;
    let mut groups = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, (separator, _)) in level.iter().enumerate().skip(1) {
        let cost = entry_size(separator.as_deref().map_or(0, <[u8]>::len));
        if used + cost > budget {
            groups.push(start..index);
            start = index;
            used = 0;
        } else {
            used += cost;
        }
    }
    groups.push(start..level.len());

    if let [.., previous, last] = groups.as_mut_slice()
        && last.len() == 1
    {
        previous.end -= 1;
        last.start -= 1;
    }

    groups
}
