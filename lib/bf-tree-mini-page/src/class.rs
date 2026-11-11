use bf_tree_page::Page;

use super::InvalidLimits;

/// Page header bytes; an empty unfenced page of capacity `c` holds `c - 32`
/// record bytes.
const HEADER_BYTES: usize = 32;

/// Mini-page capacities, ascending: powers of two from 64 below the largest
/// class `M`, then `M`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SizeClasses(Vec<usize>);

impl SizeClasses {
    /// The ladder for leaves of capacity `leaf_capacity` with keys up to
    /// `max_key_len` and values up to `max_value_len` bytes.
    ///
    /// `M = floor64(L - 5K - V - 24)`, the largest mini-page whose merge with
    /// a full base page always fits one two-way split (see the admission
    /// proof in docs/design/bf-tree-mini-page.md). Requires `M >= 64` and a
    /// maximum record, `32 + 8 + K + V` bytes with the page header, to fit
    /// in `M`.
    pub fn derive(
        leaf_capacity: usize,
        max_key_len: usize,
        max_value_len: usize,
    ) -> Result<Self, InvalidLimits> {
        Page::check_capacity(leaf_capacity)
            .map_err(|_| InvalidLimits("leaf capacity is not a valid page capacity"))?;

        let budget = max_key_len
            .checked_mul(5)
            .and_then(|keys| keys.checked_add(max_value_len))
            .and_then(|demand| demand.checked_add(24))
            .and_then(|demand| leaf_capacity.checked_sub(demand))
            .ok_or(InvalidLimits(
                "keys and values leave no room for a mini-page",
            ))?;
        let largest = budget / 64 * 64;
        if largest < 64 {
            return Err(InvalidLimits("largest mini-page class is below 64 bytes"));
        }
        if HEADER_BYTES + 8 + max_key_len + max_value_len > largest {
            return Err(InvalidLimits(
                "a maximum record does not fit the largest mini-page class",
            ));
        }

        let classes = std::iter::successors(Some(64), |class| Some(class * 2))
            .take_while(|&class| class < largest)
            .chain([largest])
            .collect();
        Ok(Self(classes))
    }

    /// The largest class, `M`.
    pub fn largest(&self) -> usize {
        *self.0.last().expect("a ladder has at least one class")
    }

    /// The class after `capacity`, or `None` at `M` or for a non-class.
    pub fn next(&self, capacity: usize) -> Option<usize> {
        let index = self.0.binary_search(&capacity).ok()?;
        self.0.get(index + 1).copied()
    }

    /// The smallest class whose empty page holds `record_bytes` (slot, key,
    /// and value), or `None` if no class does.
    pub fn smallest_for(&self, record_bytes: usize) -> Option<usize> {
        self.0
            .iter()
            .copied()
            .find(|&class| class - HEADER_BYTES >= record_bytes)
    }

    pub fn as_slice(&self) -> &[usize] {
        &self.0
    }
}
