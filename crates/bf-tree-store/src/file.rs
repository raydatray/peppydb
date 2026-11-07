use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{ENCODED_BYTES, PageStore, State, StoreError, Superblock, check_buffer};
use bf_tree_node_id::LeafId;
use bf_tree_sync::{Mutex, Table};

/// A [`PageStore`] over one file: the superblock page followed by the slots.
///
/// All I/O is synchronous and positioned. `sync` calls `File::sync_data`, so
/// a page is durable only after a `write` followed by a `sync`. Pages are
/// written in place: a torn write leaves a partial page that stored-page
/// validation rejects on the next open.
///
/// `open` does not scan for freed slots: on a large file that would cost one
/// read per slot. A reopened store treats every slot as live until a caller
/// that reads every slot, such as `Tree::open`, returns the all-zero ones
/// with [`PageStore::free`].
///
/// Reads and writes use positioned I/O and take no store-wide lock.
#[derive(Debug)]
pub struct FileStore {
    file: File,
    superblock: Superblock,
    /// One flag per slot: whether it is on the free list. Its length is the
    /// slot count.
    freed: Table<AtomicBool>,
    /// Freed slot indices, reused last-in first-out. `alloc` holds it while
    /// it extends the file, which serializes growth.
    free: Mutex<Vec<u64>>,
}

impl FileStore {
    /// Create a new store file at `path`. Fails if the path already exists.
    pub fn create(path: impl AsRef<Path>, superblock: Superblock) -> Result<Self, StoreError> {
        superblock.validate()?;

        let mut page = vec![0; superblock.page_size];
        superblock.encode(&State::default(), &mut page)?;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| io_error("create", error))?;
        write_at(&file, 0, &page).map_err(|error| io_error("write superblock", error))?;
        file.sync_all().map_err(|error| io_error("sync", error))?;

        Ok(Self {
            file,
            superblock,
            freed: Table::new(),
            free: Mutex::new(Vec::new()),
        })
    }

    /// Open an existing store file. The file length must be a whole number
    /// of pages, at least one. The free list starts empty; see the type
    /// documentation.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| io_error("open", error))?;

        let mut header = [0; ENCODED_BYTES];
        read_at(&file, 0, &mut header).map_err(|_| StoreError::InvalidSuperblock("too short"))?;
        let (superblock, _) = Superblock::decode(&header)?;

        let page_size = superblock.page_size as u64;
        let len = file
            .metadata()
            .map_err(|error| io_error("metadata", error))?
            .len();
        if len < page_size || !len.is_multiple_of(page_size) {
            return Err(StoreError::TruncatedFile);
        }
        let slots = len / page_size - 1;
        let freed = Table::new();
        freed.grow_to(usize::try_from(slots).map_err(|_| StoreError::TruncatedFile)?);

        Ok(Self {
            file,
            superblock,
            freed,
            free: Mutex::new(Vec::new()),
        })
    }

    fn offset(&self, index: u64) -> u64 {
        (index + 1) * self.superblock.page_size as u64
    }

    /// The slot index and its freed flag.
    fn slot(&self, id: LeafId) -> Result<(u64, &AtomicBool), StoreError> {
        usize::try_from(id.raw())
            .ok()
            .and_then(|index| self.freed.get(index))
            .map(|freed| (id.raw(), freed))
            .ok_or(StoreError::UnknownSlot(id))
    }
}

impl PageStore for FileStore {
    fn page_size(&self) -> usize {
        self.superblock.page_size
    }

    fn slots(&self) -> u64 {
        self.freed.len() as u64
    }

    fn alloc(&self) -> Result<LeafId, StoreError> {
        let mut free = self.free.lock();
        if let Some(index) = free.pop() {
            let (_, freed) = self.slot(LeafId::new(index).expect("stored slot"))?;
            freed.store(false, Ordering::Release);

            return Ok(LeafId::new(index).expect("slot counts stay below the reserved ID"));
        }

        // Extend the file first, so that the length stays authoritative even
        // if the process stops before the first write to the new slot.
        let index = self.freed.len() as u64;
        let zeroed = vec![0; self.superblock.page_size];
        write_at(&self.file, self.offset(index), &zeroed)
            .map_err(|error| io_error("extend", error))?;
        self.freed.grow_to(index as usize + 1);

        Ok(LeafId::new(index).expect("slot counts stay below the reserved ID"))
    }

    fn free(&self, id: LeafId) -> Result<(), StoreError> {
        let (index, freed) = self.slot(id)?;
        let mut free = self.free.lock();
        if freed.load(Ordering::Acquire) {
            return Err(StoreError::FreedSlot(id));
        }

        let zeroed = vec![0; self.superblock.page_size];
        write_at(&self.file, self.offset(index), &zeroed)
            .map_err(|error| io_error("free", error))?;
        freed.store(true, Ordering::Release);
        free.push(index);

        Ok(())
    }

    fn read(&self, id: LeafId, buf: &mut [u8]) -> Result<(), StoreError> {
        let (index, _) = self.slot(id)?;
        check_buffer(self.superblock.page_size, buf)?;

        read_at(&self.file, self.offset(index), buf).map_err(|error| io_error("read", error))
    }

    fn write(&self, id: LeafId, buf: &[u8]) -> Result<(), StoreError> {
        let (index, freed) = self.slot(id)?;
        check_buffer(self.superblock.page_size, buf)?;
        if freed.load(Ordering::Acquire) {
            return Err(StoreError::FreedSlot(id));
        }

        write_at(&self.file, self.offset(index), buf).map_err(|error| io_error("write", error))
    }

    fn sync(&self) -> Result<(), StoreError> {
        self.file
            .sync_data()
            .map_err(|error| io_error("sync", error))
    }

    fn superblock(&self) -> &Superblock {
        &self.superblock
    }
}

/// Convert an I/O error from operation `op`, keeping its kind and OS code.
fn io_error(op: &'static str, error: io::Error) -> StoreError {
    StoreError::Io {
        op,
        kind: error.kind(),
        code: error.raw_os_error(),
    }
}

fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    file.read_exact_at(buf, offset)
}

fn write_at(file: &File, offset: u64, buf: &[u8]) -> io::Result<()> {
    file.write_all_at(buf, offset)
}
