use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use bf_tree_node_id::LeafId;
use bf_tree_page::{Page, RecordState};

use super::RefBits;

/// An unfenced page whose `backing_leaf` names the leaf it overlays.
#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct MiniPage(Page);

/// The answer a mini-page gives for a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// A live value: `Insert` or `Cache`.
    Found(&'a [u8]),
    /// A known absence: `Delete` or `Phantom`.
    Absent,
    /// Not buffered; the base page decides.
    Miss,
}

impl MiniPage {
    /// Initialize an empty mini-page for `backing` over `bytes`. Panics unless
    /// `bytes.len()` is a legal page capacity, as every size class is.
    pub fn init(bytes: &mut [u8], backing: LeafId) -> &mut Self {
        let page = Page::init(bytes).expect("size classes are legal capacities");
        page.set_backing_leaf(Some(backing));
        Self::from_bytes_mut(bytes)
    }

    /// View bytes that hold a mini-page. Panics if they are too short for a
    /// page header.
    pub fn from_bytes(bytes: &[u8]) -> &Self {
        Self::ref_from_bytes(bytes).expect("mini-page bytes hold a page header")
    }

    pub fn from_bytes_mut(bytes: &mut [u8]) -> &mut Self {
        Self::mut_from_bytes(bytes).expect("mini-page bytes hold a page header")
    }

    pub fn page(&self) -> &Page {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn page_mut(&mut self) -> &mut Page {
        &mut self.0
    }

    /// The leaf this mini-page overlays.
    pub fn backing_leaf(&self) -> LeafId {
        self.0
            .backing_leaf()
            .expect("mini-pages name their backing leaf")
    }

    /// Look up `key` with one search and mark its reference bit on a hit.
    /// An empty key is a `Miss`.
    pub fn lookup_marking(&self, refs: &RefBits, key: &[u8]) -> Lookup<'_> {
        let Ok(Some(index)) = self.0.find_index(key) else {
            return Lookup::Miss;
        };
        refs.mark(index);
        match self.0.record_at(index).state {
            RecordState::Insert(value) | RecordState::Cache(value) => Lookup::Found(value),
            RecordState::Delete | RecordState::Phantom => Lookup::Absent,
        }
    }
}
