use super::Tree;
use crate::{ChildRef, Fence, Fences, InnerId, KeyRef, LeafId};

/// A structural invariant violation found by [`Tree::check_invariants`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvariantError {
    #[error("leaf {0:?} is at a different depth from the first leaf")]
    UnevenDepth(LeafId),
    #[error("inner node {0:?} has no separators")]
    EmptyInner(InnerId),
    #[error("separators of inner node {0:?} are not strictly increasing")]
    UnsortedSeparators(InnerId),
    #[error("fences of leaf {0:?} differ from its routing interval")]
    FenceMismatch(LeafId),
    #[error("leaf {0:?} holds a record outside its fences or hidden from search")]
    RecordOutOfRange(LeafId),
    #[error("{0:?} is not an allocated node")]
    Dangling(ChildRef),
    #[error("{0:?} is referenced more than once")]
    Duplicate(ChildRef),
    #[error("{0:?} is allocated but unreachable")]
    Unreachable(ChildRef),
}

impl Tree {
    /// Check the tree's structural invariants; a test and debugging aid.
    ///
    /// 1. Every leaf is at the same depth.
    /// 2. Every leaf's fences equal the routing interval its path implies.
    /// 3. Every record lies within its leaf's fences, and the leaf finds it.
    /// 4. Every inner node has strictly increasing separators, at least one.
    /// 5. Every allocated node is reachable exactly once.
    pub fn check_invariants(&self) -> Result<(), InvariantError> {
        let mut checker = Checker {
            tree: self,
            leaf_depth: None,
            seen_inner: vec![false; self.inner.len()],
            seen_leaves: vec![false; self.leaves.len()],
        };
        let unbounded = Fences {
            lower: Fence::Unbounded,
            upper: Fence::Unbounded,
        };
        checker.visit(self.root, 0, unbounded)?;

        let unreached_inner = checker
            .seen_inner
            .iter()
            .position(|seen| !seen)
            .map(|index| ChildRef::Inner(InnerId::from_index(index)));
        let unreached_leaf = checker
            .seen_leaves
            .iter()
            .position(|seen| !seen)
            .map(|index| ChildRef::Leaf(LeafId::from_index(index)));
        match unreached_inner.or(unreached_leaf) {
            Some(child) => Err(InvariantError::Unreachable(child)),
            None => Ok(()),
        }
    }
}

struct Checker<'t> {
    tree: &'t Tree,
    leaf_depth: Option<usize>,
    seen_inner: Vec<bool>,
    seen_leaves: Vec<bool>,
}

impl<'t> Checker<'t> {
    fn visit(
        &mut self,
        child: ChildRef,
        depth: usize,
        interval: Fences<KeyRef<'t>>,
    ) -> Result<(), InvariantError> {
        let seen = match child {
            ChildRef::Inner(id) => self.seen_inner.get_mut(id.index()),
            ChildRef::Leaf(id) => self.seen_leaves.get_mut(id.index()),
        };
        let seen = seen.ok_or(InvariantError::Dangling(child))?;
        if std::mem::replace(seen, true) {
            return Err(InvariantError::Duplicate(child));
        }

        match child {
            ChildRef::Leaf(id) => self.visit_leaf(id, depth, interval),
            ChildRef::Inner(id) => self.visit_inner(id, depth, interval),
        }
    }

    fn visit_leaf(
        &mut self,
        id: LeafId,
        depth: usize,
        interval: Fences<KeyRef<'t>>,
    ) -> Result<(), InvariantError> {
        if *self.leaf_depth.get_or_insert(depth) != depth {
            return Err(InvariantError::UnevenDepth(id));
        }

        let page = self.tree.leaf(id);
        if page.fences() != Some(interval) {
            return Err(InvariantError::FenceMismatch(id));
        }

        let in_range = |key: KeyRef<'_>| {
            let above = match interval.lower {
                Fence::Unbounded => true,
                Fence::Key(lower) => key >= lower,
            };
            let below = match interval.upper {
                Fence::Unbounded => true,
                Fence::Key(upper) => key < upper,
            };
            above && below
        };
        let consistent = page.iter().all(|record| {
            in_range(record.key) && page.get(&record.key.to_vec()) == Ok(Some(record))
        });
        if !consistent {
            return Err(InvariantError::RecordOutOfRange(id));
        }

        Ok(())
    }

    fn visit_inner(
        &mut self,
        id: InnerId,
        depth: usize,
        interval: Fences<KeyRef<'t>>,
    ) -> Result<(), InvariantError> {
        let node = self.tree.inner(id);
        if node.is_empty() {
            return Err(InvariantError::EmptyInner(id));
        }
        if !node
            .iter()
            .zip(node.iter().skip(1))
            .all(|(a, b)| a.key < b.key)
        {
            return Err(InvariantError::UnsortedSeparators(id));
        }

        let mut lower = interval.lower;
        let mut child = node.first_child();
        for separator in node {
            let upper = Fence::Key(separator.key);
            self.visit(child, depth + 1, Fences { lower, upper })?;
            lower = upper;
            child = separator.right_child;
        }
        let upper = interval.upper;
        self.visit(child, depth + 1, Fences { lower, upper })
    }
}
