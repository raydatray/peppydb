//! `SimStore`: an in-memory page store with a power-loss model (plan 0030,
//! from plan 0018's `FaultStore`).
//!
//! The store keeps the pages as the process sees them, a durable image,
//! and the writes since the last sync. `sync` and `commit` make writes
//! durable. A crash keeps the durable image and loses, lands, or tears
//! each later write at 64-byte sectors, chosen by the caller. The store can
//! also fail a commit before or after its superblock copy lands, and fail
//! every call from a point on, like a process that dies in the middle of
//! an operation. Clones share one store.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{COPY_BYTES, PageNumber, PageStore, State, StoreError, Superblock};

/// The unit a torn write is cut at.
pub const SECTOR: usize = 64;

/// How the next commit fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitFault {
    /// The superblock copy never lands; the old state stays durable.
    BeforeCopy,
    /// The copy lands, but the call reports an error.
    AfterCopy,
}

/// What survives a crash: the superblock, the durable pages, and the
/// durable committed state. A new `SimStore` opens it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimImage {
    pub superblock: Superblock,
    pub pages: Vec<Vec<u8>>,
    pub state: State,
}

impl SimImage {
    /// The image of a store file, as `Tree::write_snapshot` writes one: two
    /// superblock copies, then the pages.
    pub fn from_file(bytes: &[u8]) -> Result<Self, StoreError> {
        if bytes.len() < 2 * COPY_BYTES {
            return Err(StoreError::TruncatedFile);
        }
        let (copy0, rest) = bytes.split_at(COPY_BYTES);
        let (copy1, pages) = rest.split_at(COPY_BYTES);
        let (superblock, state) = Superblock::select(copy0, copy1)?;
        if !pages.len().is_multiple_of(superblock.page_size) {
            return Err(StoreError::TruncatedFile);
        }
        Ok(Self {
            superblock,
            pages: pages
                .chunks(superblock.page_size)
                .map(<[u8]>::to_vec)
                .collect(),
            state,
        })
    }
}

#[derive(Debug)]
struct Disk {
    /// The pages as the process sees them.
    pages: Vec<Vec<u8>>,
    freed: Vec<bool>,
    free: Vec<usize>,
    state: State,
    /// What survives a crash.
    durable: Vec<Vec<u8>>,
    durable_state: State,
    /// Writes since the last sync, in order.
    pending: Vec<(usize, Vec<u8>)>,
    /// Calls left before every call fails; `None` for no limit.
    countdown: Option<u64>,
    dead: bool,
    commit_fault: Option<CommitFault>,
    /// Calls so far, refused ones included.
    calls: u64,
}

/// An in-memory page store with a power-loss model.
#[derive(Clone, Debug)]
pub struct SimStore {
    disk: Arc<Mutex<Disk>>,
    superblock: Superblock,
    /// Created empty, not from an image: a tree must be created on it.
    fresh: bool,
}

fn dead() -> StoreError {
    StoreError::Io {
        op: "simulated failure",
        kind: std::io::ErrorKind::Other,
        code: Some(5),
    }
}

impl SimStore {
    /// An empty store described by `superblock`.
    pub fn new(superblock: Superblock) -> Result<Self, StoreError> {
        superblock.validate()?;
        let mut store = Self::from_image(&SimImage {
            superblock,
            pages: Vec::new(),
            state: State::default(),
        });
        store.fresh = true;
        Ok(store)
    }

    /// A store holding a crashed one's surviving image, all durable. Every
    /// page is live until the tree frees the unused ones on open, as with a
    /// reopened file.
    pub fn from_image(image: &SimImage) -> Self {
        let disk = Disk {
            pages: image.pages.clone(),
            freed: vec![false; image.pages.len()],
            free: Vec::new(),
            state: image.state,
            durable: image.pages.clone(),
            durable_state: image.state,
            pending: Vec::new(),
            countdown: None,
            dead: false,
            commit_fault: None,
            calls: 0,
        };
        Self {
            disk: Arc::new(Mutex::new(disk)),
            superblock: image.superblock,
            fresh: false,
        }
    }

    /// Whether the store was created empty, so a tree must be created on it
    /// rather than opened.
    pub fn is_fresh(&self) -> bool {
        self.fresh
    }

    fn lock(&self) -> MutexGuard<'_, Disk> {
        self.disk.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fail every call after `calls` more.
    pub fn die_after(&self, calls: u64) {
        self.lock().countdown = Some(calls);
    }

    /// Remove a [`SimStore::die_after`] limit that has not run out.
    pub fn live_on(&self) {
        self.lock().countdown = None;
    }

    /// Fail every later call, as a process that died.
    pub fn kill(&self) {
        self.lock().dead = true;
    }

    pub fn fail_next_commit(&self, fault: CommitFault) {
        self.lock().commit_fault = Some(fault);
    }

    /// Calls so far, refused ones included.
    pub fn calls(&self) -> u64 {
        self.lock().calls
    }

    /// The committed state that survives a crash.
    pub fn durable_state(&self) -> State {
        self.lock().durable_state
    }

    /// What survives a crash now: the durable image, with each write since
    /// the last sync lost, landed, or torn at 64-byte sectors. `below(n)`
    /// returns a choice in `0..n`: 0 loses a write, 1 lands it, 2 tears
    /// it; when torn, `below(2)` per sector picks the sectors that land.
    pub fn crash_with(&self, mut below: impl FnMut(u64) -> u64) -> SimImage {
        let disk = self.lock();
        let mut durable = disk.durable.clone();
        for (index, bytes) in &disk.pending {
            match below(3) {
                0 => {}
                1 => durable[*index].clone_from(bytes),
                _ => {
                    for sector in 0..bytes.len() / SECTOR {
                        if below(2) == 0 {
                            let range = sector * SECTOR..(sector + 1) * SECTOR;
                            durable[*index][range.clone()].copy_from_slice(&bytes[range]);
                        }
                    }
                }
            }
        }
        SimImage {
            superblock: self.superblock,
            pages: durable,
            state: disk.durable_state,
        }
    }

    /// What survives a crash now, with choices drawn from `seed`.
    pub fn crash(&self, seed: u64) -> SimImage {
        let mut state = seed | 1;
        self.crash_with(|bound| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        })
    }
}

impl Disk {
    /// Count one call; `Err` once the process has died.
    fn call(&mut self) -> Result<(), StoreError> {
        self.calls += 1;
        if let Some(left) = self.countdown.as_mut() {
            if *left == 0 {
                self.dead = true;
            } else {
                *left -= 1;
            }
        }
        if self.dead { Err(dead()) } else { Ok(()) }
    }

    fn sync(&mut self) {
        for (index, bytes) in self.pending.drain(..) {
            self.durable[index] = bytes;
        }
    }

    fn index(&self, page_number: PageNumber) -> Result<usize, StoreError> {
        let index = page_number.raw() as usize;
        if index < self.pages.len() {
            Ok(index)
        } else {
            Err(StoreError::UnknownPage(page_number))
        }
    }
}

impl PageStore for SimStore {
    fn page_size(&self) -> usize {
        self.superblock.page_size
    }

    fn page_count(&self) -> u64 {
        self.lock().pages.len() as u64
    }

    fn alloc(&self) -> Result<PageNumber, StoreError> {
        let mut disk = self.lock();
        disk.call()?;
        let index = match disk.free.pop() {
            Some(index) => {
                disk.freed[index] = false;
                index
            }
            None => {
                // The file grows with a zero page.
                let zero = vec![0; self.superblock.page_size];
                disk.pages.push(zero.clone());
                disk.durable.push(zero);
                disk.freed.push(false);
                disk.pages.len() - 1
            }
        };
        PageNumber::new(index as u64).ok_or(StoreError::InvalidSuperblock("page count overflow"))
    }

    fn free(&self, page_number: PageNumber) -> Result<(), StoreError> {
        let mut disk = self.lock();
        disk.call()?;
        let index = disk.index(page_number)?;
        if disk.freed[index] {
            return Err(StoreError::FreedPage(page_number));
        }
        disk.freed[index] = true;
        disk.free.push(index);
        Ok(())
    }

    fn read(&self, page_number: PageNumber, buf: &mut [u8]) -> Result<(), StoreError> {
        let mut disk = self.lock();
        disk.call()?;
        let index = disk.index(page_number)?;
        if buf.len() != self.superblock.page_size {
            return Err(StoreError::BufferLength {
                expected: self.superblock.page_size,
                actual: buf.len(),
            });
        }
        buf.copy_from_slice(&disk.pages[index]);
        Ok(())
    }

    fn write(&self, page_number: PageNumber, buf: &[u8]) -> Result<(), StoreError> {
        let mut disk = self.lock();
        disk.call()?;
        let index = disk.index(page_number)?;
        if disk.freed[index] {
            return Err(StoreError::FreedPage(page_number));
        }
        if buf.len() != self.superblock.page_size {
            return Err(StoreError::BufferLength {
                expected: self.superblock.page_size,
                actual: buf.len(),
            });
        }
        disk.pages[index].copy_from_slice(buf);
        disk.pending.push((index, buf.to_vec()));
        Ok(())
    }

    fn sync(&self) -> Result<(), StoreError> {
        let mut disk = self.lock();
        disk.call()?;
        disk.sync();
        Ok(())
    }

    fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    fn state(&self) -> State {
        self.lock().state
    }

    fn commit(&self, state: &State) -> Result<(), StoreError> {
        let mut disk = self.lock();
        disk.call()?;
        if disk.state.checkpoint_id + 1 != state.checkpoint_id {
            return Err(StoreError::CommitOrder {
                current: disk.state.checkpoint_id,
                got: state.checkpoint_id,
            });
        }
        // A commit writes a superblock copy and syncs the file, which also
        // makes every earlier page write durable.
        match disk.commit_fault.take() {
            Some(CommitFault::BeforeCopy) => Err(dead()),
            Some(CommitFault::AfterCopy) => {
                disk.sync();
                disk.durable_state = *state;
                Err(dead())
            }
            None => {
                disk.sync();
                disk.durable_state = *state;
                disk.state = *state;
                Ok(())
            }
        }
    }
}
