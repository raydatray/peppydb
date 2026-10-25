use super::{PageStore, StoreError, Superblock, check_buffer, slot_id};
use crate::LeafId;

/// An in-memory [`PageStore`]: one boxed buffer per slot and a free list.
///
/// This is the fast test backend and the store behind `Tree::in_memory`. `sync` is
/// a no-op; nothing outlives the value.
#[derive(Debug)]
pub struct MemStore {
    superblock: Superblock,
    pages: Vec<Box<[u8]>>,
    /// Parallel to `pages`: whether the slot is on the free list.
    freed: Vec<bool>,
    /// Freed slot indices, reused last-in first-out.
    free: Vec<usize>,
}

impl MemStore {
    /// Create an empty store described by `superblock`.
    pub fn new(superblock: Superblock) -> Result<Self, StoreError> {
        superblock.validate()?;

        Ok(Self {
            superblock,
            pages: Vec::new(),
            freed: Vec::new(),
            free: Vec::new(),
        })
    }

    fn index(&self, id: LeafId) -> Result<usize, StoreError> {
        usize::try_from(id.raw())
            .ok()
            .filter(|&index| index < self.pages.len())
            .ok_or(StoreError::UnknownSlot(id))
    }
}

impl PageStore for MemStore {
    fn page_size(&self) -> usize {
        self.superblock.page_size
    }

    fn slots(&self) -> u64 {
        self.pages.len() as u64
    }

    fn alloc(&mut self) -> Result<LeafId, StoreError> {
        if let Some(index) = self.free.pop() {
            self.freed[index] = false;

            return Ok(slot_id(index as u64));
        }

        let index = self.pages.len();
        self.pages
            .push(vec![0; self.superblock.page_size].into_boxed_slice());
        self.freed.push(false);

        Ok(slot_id(index as u64))
    }

    fn free(&mut self, id: LeafId) -> Result<(), StoreError> {
        let index = self.index(id)?;
        if self.freed[index] {
            return Err(StoreError::FreedSlot(id));
        }

        self.pages[index].fill(0);
        self.freed[index] = true;
        self.free.push(index);

        Ok(())
    }

    fn read(&self, id: LeafId, buf: &mut [u8]) -> Result<(), StoreError> {
        let index = self.index(id)?;
        check_buffer(self.superblock.page_size, buf)?;

        buf.copy_from_slice(&self.pages[index]);

        Ok(())
    }

    fn write(&mut self, id: LeafId, buf: &[u8]) -> Result<(), StoreError> {
        let index = self.index(id)?;
        check_buffer(self.superblock.page_size, buf)?;
        if self.freed[index] {
            return Err(StoreError::FreedSlot(id));
        }

        self.pages[index].copy_from_slice(buf);

        Ok(())
    }

    fn sync(&mut self) -> Result<(), StoreError> {
        Ok(())
    }

    fn superblock(&self) -> &Superblock {
        &self.superblock
    }
}
