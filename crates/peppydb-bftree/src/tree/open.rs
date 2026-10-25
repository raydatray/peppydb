//! Rebuilding the routing layer from leaf fences.
//!
//! Inner nodes are derived data. Given every leaf's ID and fences, `tile`
//! checks that the fences partition the key space and `build_inner` packs
//! the separators into inner levels bottom-up. Both are pure functions over
//! fence lists, so they are tested without a store.

use std::cmp::Ordering;
use std::ops::Range;

use super::TreeError;
use super::store::InnerStore;
use crate::inner::{HEADER_BYTES, InnerError, InnerNode, entry_size};
use crate::{ChildRef, LeafId};

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
