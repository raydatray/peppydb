use std::cell::RefCell;
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
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// How a [`FileStore`] reaches its file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Io {
    /// Through the page cache. Portable. The kernel caches pages a second
    /// time, outside the circular buffer's budget.
    #[default]
    Cached,
    /// Bypassing the page cache (`O_DIRECT`). Linux only, 4096-byte pages.
    /// The circular buffer's budget then bounds the memory the data
    /// occupies, and a circular-buffer miss reaches the device. Durability
    /// still needs `sync`.
    Direct,
}

/// The alignment `O_DIRECT` needs for buffers, offsets, and lengths. It
/// covers devices with 512-byte and 4096-byte logical blocks.
const DIRECT_ALIGN: usize = 4096;

/// One page aligned for `O_DIRECT`. Alignment is a property of the type, so
/// the bounce buffer needs no `unsafe`.
#[derive(Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, align(4096))]
struct AlignedPage([u8; DIRECT_ALIGN]);

thread_local! {
    /// Reused per thread; grows to the largest transfer made through it.
    static BOUNCE: RefCell<Vec<AlignedPage>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` with an aligned buffer of `len` bytes (a multiple of 4096).
fn with_bounce<R>(len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    debug_assert!(len.is_multiple_of(DIRECT_ALIGN));
    BOUNCE.with(|bounce| {
        let mut pages = bounce.borrow_mut();
        let count = len / DIRECT_ALIGN;
        if pages.len() < count {
            pages.resize(count, AlignedPage([0; DIRECT_ALIGN]));
        }
        f(&mut pages[..count].as_mut_bytes()[..len])
    })
}

fn aligned(buf: &[u8]) -> bool {
    buf.as_ptr().addr().is_multiple_of(DIRECT_ALIGN) && buf.len().is_multiple_of(DIRECT_ALIGN)
}

/// Open `path` for `io`. Direct I/O is Linux only, and needs 4096-byte pages
/// (checked by the callers once the page size is known).
fn open_file(options: &mut OpenOptions, path: &Path, io: Io) -> io::Result<File> {
    if io == Io::Direct {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_DIRECT);
        }
    }
    options.open(path)
}

fn check_direct(io: Io, page_size: usize) -> Result<(), StoreError> {
    if io == Io::Direct {
        if !cfg!(target_os = "linux") {
            return Err(StoreError::DirectIo("direct I/O requires Linux"));
        }
        if page_size != DIRECT_ALIGN {
            return Err(StoreError::DirectIo("direct I/O requires 4096-byte pages"));
        }
    }
    Ok(())
}

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
    io: Io,
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
    /// Create a new store file at `path` with cached I/O. Fails if the path
    /// already exists.
    pub fn create(path: impl AsRef<Path>, superblock: Superblock) -> Result<Self, StoreError> {
        Self::create_with(path, superblock, Io::Cached)
    }

    /// [`FileStore::create`] with a chosen I/O mode.
    pub fn create_with(
        path: impl AsRef<Path>,
        superblock: Superblock,
        io: Io,
    ) -> Result<Self, StoreError> {
        superblock.validate()?;
        check_direct(io, superblock.page_size)?;

        let mut block = vec![0; COPY_BYTES];
        superblock.encode(&State::default(), &mut block)?;

        let file = open_file(
            OpenOptions::new().read(true).write(true).create_new(true),
            path.as_ref(),
            io,
        )
        .map_err(|error| io_error("create", error))?;
        for copy in 0..2 {
            write_at(&file, io, copy * COPY_BYTES as u64, &block)
                .map_err(|error| io_error("write superblock", error))?;
        }
        file.sync_all().map_err(|error| io_error("sync", error))?;

        Ok(Self::with(
            file,
            io,
            superblock,
            State::default(),
            Table::new(),
        ))
    }

    /// Open an existing store file: select the newer intact superblock copy,
    /// and check that the file holds the superblock area and a whole number
    /// of pages, with cached I/O. The free list starts empty; see the type
    /// documentation.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with(path, Io::Cached)
    }

    /// [`FileStore::open`] with a chosen I/O mode. A file written in one mode
    /// opens in the other: the format does not depend on it.
    pub fn open_with(path: impl AsRef<Path>, io: Io) -> Result<Self, StoreError> {
        // The page size is checked once the superblock is read.
        check_direct(io, DIRECT_ALIGN)?;
        let file = open_file(OpenOptions::new().read(true).write(true), path.as_ref(), io)
            .map_err(|error| io_error("open", error))?;

        let len = file
            .metadata()
            .map_err(|error| io_error("metadata", error))?
            .len();
        if len < PAGES_OFFSET {
            return Err(StoreError::TruncatedFile);
        }
        let mut copies = vec![0; 2 * COPY_BYTES];
        read_at(&file, io, 0, &mut copies).map_err(|error| io_error("read superblock", error))?;
        let (copy0, copy1) = copies.split_at(COPY_BYTES);
        let (superblock, state) = Superblock::select(copy0, copy1)?;
        check_direct(io, superblock.page_size)?;

        let page_size = superblock.page_size as u64;
        if !(len - PAGES_OFFSET).is_multiple_of(page_size) {
            return Err(StoreError::TruncatedFile);
        }
        let pages = (len - PAGES_OFFSET) / page_size;
        let freed = Table::new();
        freed.grow_to(usize::try_from(pages).map_err(|_| StoreError::TruncatedFile)?);

        Ok(Self::with(file, io, superblock, state, freed))
    }

    fn with(
        file: File,
        io: Io,
        superblock: Superblock,
        state: State,
        freed: Table<AtomicBool>,
    ) -> Self {
        Self {
            file,
            io,
            superblock,
            freed,
            free: Mutex::new(Vec::new()),
            commit: Mutex::new(()),
            state: Mutex::new(state),
        }
    }

    /// The I/O mode the store was opened with.
    pub fn io(&self) -> Io {
        self.io
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
        write_at(&self.file, self.io, self.offset(index), &zeroed)
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

        read_at(&self.file, self.io, self.offset(index), buf)
            .map_err(|error| io_error("read", error))
    }

    fn write(&self, page_number: PageNumber, buf: &[u8]) -> Result<(), StoreError> {
        let (index, freed) = self.page(page_number)?;
        check_buffer(self.superblock.page_size, buf)?;
        if freed.load(Ordering::Acquire) {
            return Err(StoreError::FreedPage(page_number));
        }

        write_at(&self.file, self.io, self.offset(index), buf)
            .map_err(|error| io_error("write", error))
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
        write_at(&self.file, self.io, copy * COPY_BYTES as u64, &block)
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

/// Read `buf` at `offset`. With direct I/O, an unaligned buffer goes
/// through the bounce buffer.
fn read_at(file: &File, io: Io, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    if io == Io::Cached || aligned(buf) {
        return file.read_exact_at(buf, offset);
    }
    with_bounce(buf.len(), |bounce| {
        file.read_exact_at(bounce, offset)?;
        buf.copy_from_slice(bounce);
        Ok(())
    })
}

/// Write `buf` at `offset`. With direct I/O, an unaligned buffer goes
/// through the bounce buffer.
fn write_at(file: &File, io: Io, offset: u64, buf: &[u8]) -> io::Result<()> {
    if io == Io::Cached || aligned(buf) {
        return file.write_all_at(buf, offset);
    }
    with_bounce(buf.len(), |bounce| {
        bounce.copy_from_slice(buf);
        file.write_all_at(bounce, offset)
    })
}

/// The file handle, for tests that check `O_DIRECT` is in effect.
#[cfg(all(test, target_os = "linux"))]
impl FileStore {
    pub(crate) fn file(&self) -> &File {
        &self.file
    }
}
