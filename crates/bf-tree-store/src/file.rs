use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    COPY_BYTES, PAGES_OFFSET, PageNumber, PageStore, State, StoreError, Superblock, check_buffer,
    check_commit_order,
};
use bf_tree_sync::{Mutex, Table};

/// A [`PageStore`] over one file: two 4096-byte superblock copies, then the
/// pages. Page number `i` lives at byte `PAGES_OFFSET + i * page_size`.
///
/// A commit writes copy `checkpoint_id % 2` and syncs. The previous commit
/// is in the other copy, so a torn commit leaves it intact, and `open`
/// selects the newer intact copy.
///
/// All I/O is synchronous and positioned. `sync` calls `File::sync_data`, so
/// a page is durable only after a `write` followed by a `sync`. Pages are
/// written in place: a torn write leaves a partial page whose checksum the
/// tree rejects on the next open.
///
/// `open` does not scan for freed pages: on a large file that would cost one
/// read per page. A reopened store treats every page as live until a caller
/// that reads every page, such as `Tree::open`, returns the unused ones with
/// [`PageStore::free`].
///
/// Reads and writes use positioned I/O and take no store-wide lock.
#[derive(Debug)]
pub struct FileStore {
    file: File,
    superblock: Superblock,
    /// One flag per page: whether it is on the free list. Its length is the
    /// page count.
    freed: Table<AtomicBool>,
    /// Freed page indices, reused last-in first-out. `alloc` holds it while
    /// it extends the file, which serializes growth.
    free: Mutex<Vec<u64>>,
    /// Serializes commits; held across the copy write and its sync.
    commit: Mutex<()>,
    /// The last committed state; held only briefly, so `state()` never
    /// waits for a commit's sync.
    state: Mutex<State>,
}

impl FileStore {
    /// Create a new store file at `path`. Fails if the path already exists.
    pub fn create(path: impl AsRef<Path>, superblock: Superblock) -> Result<Self, StoreError> {
        superblock.validate()?;

        let mut block = vec![0; COPY_BYTES];
        superblock.encode(&State::default(), &mut block)?;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| io_error("create", error))?;
        for copy in 0..2 {
            write_at(&file, copy * COPY_BYTES as u64, &block)
                .map_err(|error| io_error("write superblock", error))?;
        }
        file.sync_all().map_err(|error| io_error("sync", error))?;

        Ok(Self::with(file, superblock, State::default(), Table::new()))
    }

    /// Open an existing store file: select the newer intact superblock copy,
    /// and check that the file holds the superblock area and a whole number
    /// of pages. The free list starts empty; see the type documentation.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| io_error("open", error))?;

        let len = file
            .metadata()
            .map_err(|error| io_error("metadata", error))?
            .len();
        if len < PAGES_OFFSET {
            return Err(StoreError::TruncatedFile);
        }
        let mut copies = vec![0; 2 * COPY_BYTES];
        read_at(&file, 0, &mut copies).map_err(|error| io_error("read superblock", error))?;
        let (copy0, copy1) = copies.split_at(COPY_BYTES);
        let (superblock, state) = Superblock::select(copy0, copy1)?;

        let page_size = superblock.page_size as u64;
        if !(len - PAGES_OFFSET).is_multiple_of(page_size) {
            return Err(StoreError::TruncatedFile);
        }
        let pages = (len - PAGES_OFFSET) / page_size;
        let freed = Table::new();
        freed.grow_to(usize::try_from(pages).map_err(|_| StoreError::TruncatedFile)?);

        Ok(Self::with(file, superblock, state, freed))
    }

    fn with(file: File, superblock: Superblock, state: State, freed: Table<AtomicBool>) -> Self {
        Self {
            file,
            superblock,
            freed,
            free: Mutex::new(Vec::new()),
            commit: Mutex::new(()),
            state: Mutex::new(state),
        }
    }

    fn offset(&self, index: u64) -> u64 {
        PAGES_OFFSET + index * self.superblock.page_size as u64
    }

    /// The page's index and its freed flag.
    fn page(&self, page_number: PageNumber) -> Result<(u64, &AtomicBool), StoreError> {
        page_number
            .index()
            .and_then(|index| self.freed.get(index))
            .map(|freed| (page_number.raw(), freed))
            .ok_or(StoreError::UnknownPage(page_number))
    }
}

impl PageStore for FileStore {
    fn page_size(&self) -> usize {
        self.superblock.page_size
    }

    fn page_count(&self) -> u64 {
        self.freed.len() as u64
    }

    fn alloc(&self) -> Result<PageNumber, StoreError> {
        let mut free = self.free.lock();
        if let Some(index) = free.pop() {
            let page_number = PageNumber::new(index).expect("a stored page");
            let (_, freed) = self.page(page_number)?;
            freed.store(false, Ordering::Release);

            return Ok(page_number);
        }

        // Extend the file first, so that the length stays authoritative even
        // if the process stops before the first write to the new page.
        let index = self.freed.len() as u64;
        let zeroed = vec![0; self.superblock.page_size];
        write_at(&self.file, self.offset(index), &zeroed)
            .map_err(|error| io_error("extend", error))?;
        self.freed.grow_to(index as usize + 1);

        Ok(PageNumber::new(index).expect("page counts stay below u64::MAX"))
    }

    fn free(&self, page_number: PageNumber) -> Result<(), StoreError> {
        let (index, freed) = self.page(page_number)?;
        let mut free = self.free.lock();
        if freed.load(Ordering::Acquire) {
            return Err(StoreError::FreedPage(page_number));
        }

        freed.store(true, Ordering::Release);
        free.push(index);

        Ok(())
    }

    fn read(&self, page_number: PageNumber, buf: &mut [u8]) -> Result<(), StoreError> {
        let (index, _) = self.page(page_number)?;
        check_buffer(self.superblock.page_size, buf)?;

        read_at(&self.file, self.offset(index), buf).map_err(|error| io_error("read", error))
    }

    fn write(&self, page_number: PageNumber, buf: &[u8]) -> Result<(), StoreError> {
        let (index, freed) = self.page(page_number)?;
        check_buffer(self.superblock.page_size, buf)?;
        if freed.load(Ordering::Acquire) {
            return Err(StoreError::FreedPage(page_number));
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

    fn state(&self) -> State {
        *self.state.lock()
    }

    fn commit(&self, state: &State) -> Result<(), StoreError> {
        let _commit = self.commit.lock();
        check_commit_order(&self.state(), state)?;
        let mut block = vec![0; COPY_BYTES];
        self.superblock.encode(state, &mut block)?;
        let copy = state.checkpoint_id % 2;
        write_at(&self.file, copy * COPY_BYTES as u64, &block)
            .map_err(|error| io_error("commit", error))?;
        self.file
            .sync_data()
            .map_err(|error| io_error("sync", error))?;
        *self.state.lock() = *state;
        Ok(())
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
