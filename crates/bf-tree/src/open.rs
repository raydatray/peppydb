//! Reopening a tree from its stored leaves.
//!
//! Inner nodes are derived data. `Tree::open` reads every page, checks its
//! checksum and structure, maps the leaf it names to its page number, then
//! `tile` checks that the leaf fences partition the key space and
//! `build_inner` packs the separators into inner levels bottom-up. The last
//! two are pure functions over fence lists, so they are tested without a
//! store.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::hash_map::Entry as Slot;
use std::ops::Range;

use super::inner::Inner;
use super::leaf_map::LeafMap;
use super::{Tree, TreeConfig, TreeError};
use bf_tree_inner::{HEADER_BYTES, InnerError, InnerNode, entry_size};
use bf_tree_node_id::{ChildRef, LeafId};
use bf_tree_page::{Fence, KeyRef, Page, PageError};
use bf_tree_store::{PageNumber, PageStore, StoreError, Superblock};

impl Tree {
    /// Open a tree from a store written by [`Tree::create`] and
    /// [`Tree::flush`], rebuilding the routing layer from leaf fences.
    ///
    /// The configuration comes from the store's superblock. Every page is
    /// read once: all-zero pages are free (allocated but never written) and
    /// are returned to the store for reuse; every other page must pass its
    /// checksum and [`Page::validate`] (else `CorruptPage`), name a leaf that
    /// no other page names, carry fences, and respect the configured key and
    /// value limits. The leaves must tile the key space, so a store with a
    /// missing, duplicated, or torn leaf is rejected rather than opened. Leaf
    /// IDs that no page names are holes and stay unused. Open is `O(pages)`
    /// in I/O and memory.
    pub fn open(store: Box<dyn PageStore>) -> Result<Self, TreeError> {
        let config = TreeConfig::from(store.superblock());
        Self::open_with(config, store)
    }

    /// [`Tree::open`] with chosen runtime settings, such as the
    /// circular-buffer budget. The layout settings in `config` must match the
    /// store's superblock.
    pub fn open_with(config: TreeConfig, store: Box<dyn PageStore>) -> Result<Self, TreeError> {
        if Superblock::from(&config) != *store.superblock() {
            return Err(TreeError::InvalidStore(
                "store superblock differs from the configuration",
            ));
        }
        config.validate()?;

        let state = store.state();
        let selection = select(store.as_ref(), &config, state.write_seq)?;
        // Pages written after the committed checkpoint must not reappear
        // after a later commit: zero them, sync, and only then release them.
        if !selection.erased.is_empty() {
            let zeros = vec![0; store.page_size()];
            for page_number in &selection.erased {
                store.write(*page_number, &zeros)?;
            }
            store.sync()?;
        }
        // Older copies of a leaf are superseded: no commit needs them.
        for page_number in selection.released.iter().chain(&selection.erased) {
            match store.free(*page_number) {
                Ok(()) | Err(StoreError::FreedPage(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let mut pages: Vec<Option<PageNumber>> = Vec::new();
        let mut leaves = Vec::new();
        for (id, chosen) in selection.chosen {
            if pages.len() <= id.index() {
                pages.resize(id.index() + 1, None);
            }
            pages[id.index()] = Some(chosen.page_number);
            leaves.push(chosen.bounds);
        }

        let leaves = tile(leaves)?;
        let inner = Inner::new(config.inner_capacity, ChildRef::Leaf(leaves[0].id));
        inner.set_root(build_inner(&inner, &leaves)?);

        let leaves = LeafMap::for_config(store, pages, &config)?;
        // Keep every write sequence in the file unique, including erased
        // ones whose zeroing may not have landed.
        leaves.start_write_seq(selection.max_seen.max(state.write_seq));
        Ok(Self::assemble(config, inner, leaves))
    }
}

/// The page that open keeps for one leaf.
struct Chosen {
    write_seq: u64,
    page_number: PageNumber,
    bounds: LeafBounds,
}

/// What a scan of every stored page selected.
struct Selection {
    /// The newest page of each leaf, by leaf ID.
    chosen: HashMap<LeafId, Chosen>,
    /// Pages to free: older copies of a leaf.
    released: Vec<PageNumber>,
    /// Pages written after the committed write watermark, to zero and free.
    erased: Vec<PageNumber>,
    /// The largest write sequence in any page.
    max_seen: u64,
}

/// Read every page and keep the newest committed page of each leaf: the one
/// with the highest write sequence at or below `written`, the committed
/// write watermark. Pages above it are erased. All-zero pages were
/// allocated and never written, and are freed at once.
fn select(
    store: &dyn PageStore,
    config: &TreeConfig,
    written: u64,
) -> Result<Selection, TreeError> {
    let page_count = store.page_count();
    // Leaf IDs from disk are bounded before any table grows, so a damaged
    // stamp cannot make open allocate a huge table. IDs can exceed the page
    // count (a discarded split half uses an ID, and its page is reused), but
    // not by more than a generous margin.
    let max_leaf = page_count.saturating_mul(4).max(1 << 16);
    let mut selection = Selection {
        chosen: HashMap::new(),
        released: Vec::new(),
        erased: Vec::new(),
        max_seen: 0,
    };
    let mut bytes = vec![0; store.page_size()];
    for raw in 0..page_count {
        let page_number = PageNumber::new(raw).expect("page counts stay below u64::MAX");
        store.read(page_number, &mut bytes)?;
        if bytes.iter().all(|&byte| byte == 0) {
            // A page allocated but never written. Return it to the store's
            // free list; a backend that rescanned on open already has it.
            match store.free(page_number) {
                Ok(()) | Err(StoreError::FreedPage(_)) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let (write_seq, _) = bf_tree_page::raw_header_stamps(&bytes).expect("a whole page");
        selection.max_seen = selection.max_seen.max(write_seq);
        if write_seq > written {
            // Written after the checkpoint; whatever it holds, a crash can
            // have torn it.
            selection.erased.push(page_number);
            continue;
        }
        let (id, bounds) = check_page(&bytes, page_number, config, max_leaf)?;
        let candidate = Chosen {
            write_seq,
            page_number,
            bounds,
        };
        match selection.chosen.entry(id) {
            Slot::Vacant(entry) => {
                entry.insert(candidate);
            }
            Slot::Occupied(mut entry) => {
                let current = entry.get().write_seq;
                if current == write_seq {
                    return Err(TreeError::InconsistentLeaves(
                        "two stored pages name the same leaf",
                    ));
                }
                let older = if write_seq > current {
                    entry.insert(candidate)
                } else {
                    candidate
                };
                selection.released.push(older.page_number);
            }
        }
    }
    Ok(selection)
}

/// Check one stored page: its checksum and structure (`CorruptPage`), its
/// leaf stamp, fences, and limits. Returns its leaf and bounds.
fn check_page(
    bytes: &[u8],
    page_number: PageNumber,
    config: &TreeConfig,
    max_leaf: u64,
) -> Result<(LeafId, LeafBounds), TreeError> {
    let corrupt = |reason| TreeError::CorruptPage {
        page_number,
        reason,
    };
    if !bf_tree_page::verify_stored_checksum(bytes) {
        return Err(corrupt("checksum mismatch"));
    }
    let page = Page::validate(bytes).map_err(|error| match error {
        PageError::Corrupt(reason) => corrupt(reason),
        other => TreeError::Page(other),
    })?;
    let id = page
        .leaf()
        .ok_or(TreeError::InconsistentLeaves("a stored page names no leaf"))?;
    if id.raw() >= max_leaf {
        return Err(TreeError::InconsistentLeaves(
            "a stored page names an implausibly large leaf ID",
        ));
    }

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
    Ok((
        id,
        LeafBounds {
            id,
            lower: bound(fences.lower)?,
            upper: bound(fences.upper)?,
        },
    ))
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
pub(super) fn build_inner(inner: &Inner, leaves: &[LeafBounds]) -> Result<ChildRef, TreeError> {
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
            let id = inner.alloc_with(|bytes| {
                let node = InnerNode::init(bytes, *first)?;
                entries[1..].iter().try_for_each(|(separator, child)| {
                    let key = separator
                        .as_deref()
                        .expect("every child after the first has a separator");
                    node.try_insert(key, *child)
                })
            })?;
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
