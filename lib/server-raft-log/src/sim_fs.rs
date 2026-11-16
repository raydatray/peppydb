//! `SimFs`: an in-memory file system with a power-loss model (plan 0029,
//! "The file-system trait").
//!
//! Each file has live contents, which reads see, and durable contents,
//! which survive a crash. Each directory has live and durable names.
//!
//! - `sync_data` makes a file's live contents and length durable.
//! - `sync_dir` makes the directory's live names durable: names created,
//!   renamed, and removed before it survive a crash; later ones do not.
//! - On a crash, a file whose live contents only extend its durable
//!   contents keeps a prefix of the extension, chosen from the seed: a torn
//!   tail. Any other unsynced change (a shortening) is lost whole.
//! - Faults: every mutating operation numbered at or above a power-cut
//!   point fails with no effect, so a test can stop at every operation in
//!   turn; and one chosen operation can fail by itself.
//!
//! The model treats a whole write as one unit that can be cut anywhere,
//! which is stricter than sector-atomic writes.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::fs::{Fs, FsFile};

/// One file's contents.
#[derive(Clone, Debug, Default)]
struct Inode {
    live: Vec<u8>,
    durable: Vec<u8>,
}

/// The surviving state of a crashed [`SimFs`]: each durable name and its
/// contents. A new `SimFs` opens it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SimImage {
    pub files: BTreeMap<PathBuf, Vec<u8>>,
}

#[derive(Debug, Default)]
struct State {
    inodes: Vec<Inode>,
    live: BTreeMap<PathBuf, usize>,
    durable: BTreeMap<PathBuf, usize>,
    /// Mutating operations performed or refused so far.
    ops: u64,
    /// Operations numbered at or above this fail with no effect.
    power_cut: Option<u64>,
    /// The operation with this number fails with no effect.
    fail_op: Option<u64>,
}

impl State {
    /// Count one mutating operation, or refuse it.
    fn begin(&mut self, what: &'static str) -> io::Result<()> {
        let op = self.ops;
        self.ops += 1;
        if self.power_cut.is_some_and(|cut| op >= cut) || self.fail_op == Some(op) {
            return Err(io::Error::other(format!("injected failure: {what}")));
        }
        Ok(())
    }

    fn inode(&self, path: &Path) -> io::Result<usize> {
        self.live
            .get(path)
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))
    }
}

/// An in-memory file system with a power-loss model. Clones share one file
/// system.
#[derive(Clone, Debug, Default)]
pub struct SimFs {
    state: Arc<Mutex<State>>,
}

impl SimFs {
    /// An empty file system.
    pub fn new() -> Self {
        Self::default()
    }

    /// A file system holding a crashed one's surviving image, all durable.
    pub fn from_image(image: &SimImage) -> Self {
        let mut state = State::default();
        for (path, bytes) in &image.files {
            let id = state.inodes.len();
            state.inodes.push(Inode {
                live: bytes.clone(),
                durable: bytes.clone(),
            });
            state.live.insert(path.clone(), id);
            state.durable.insert(path.clone(), id);
        }
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Mutating operations so far, refused ones included.
    pub fn ops(&self) -> u64 {
        self.lock().ops
    }

    /// Cut the power at operation `op`: it and every later mutating
    /// operation fail with no effect. `None` restores the power.
    pub fn cut_power_at(&self, op: Option<u64>) {
        self.lock().power_cut = op;
    }

    /// Make operation `op` (an absolute number, see [`SimFs::ops`]) fail
    /// with no effect.
    pub fn fail_op(&self, op: Option<u64>) {
        self.lock().fail_op = op;
    }

    /// What survives a crash now. Unsynced extensions of a file are torn at
    /// a point drawn by `choose`, which gets the extension's length and
    /// returns how many of its bytes survive.
    pub fn crash_with(&self, mut choose: impl FnMut(usize) -> usize) -> SimImage {
        let state = self.lock();
        let files = state
            .durable
            .iter()
            .map(|(path, id)| {
                let inode = &state.inodes[*id];
                let mut bytes = inode.durable.clone();
                if let Some(extension) = inode.live.strip_prefix(inode.durable.as_slice()) {
                    let keep = choose(extension.len()).min(extension.len());
                    bytes.extend(&extension[..keep]);
                }
                (path.clone(), bytes)
            })
            .collect();
        SimImage { files }
    }

    /// What survives a crash now, with tears drawn from `seed`.
    pub fn crash(&self, seed: u64) -> SimImage {
        let mut rng = SplitMix(seed);
        self.crash_with(|len| (rng.next() % (len as u64 + 1)) as usize)
    }

    /// What survives a crash if no unsynced byte does.
    pub fn crash_clean(&self) -> SimImage {
        self.crash_with(|_| 0)
    }
}

/// SplitMix64: a small deterministic stream for crash choices.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// A file of [`SimFs`].
#[derive(Debug)]
pub struct SimFile {
    fs: SimFs,
    id: usize,
}

impl Fs for SimFs {
    type File = SimFile;

    fn create_dir_all(&self, _dir: &Path) -> io::Result<()> {
        // Directories exist implicitly.
        Ok(())
    }

    fn create_new(&self, path: &Path) -> io::Result<SimFile> {
        let mut state = self.lock();
        if state.live.contains_key(path) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                path.display().to_string(),
            ));
        }
        state.begin("create")?;
        let id = state.inodes.len();
        state.inodes.push(Inode::default());
        state.live.insert(path.to_path_buf(), id);
        Ok(SimFile {
            fs: self.clone(),
            id,
        })
    }

    fn open(&self, path: &Path) -> io::Result<SimFile> {
        let id = self.lock().inode(path)?;
        Ok(SimFile {
            fs: self.clone(),
            id,
        })
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let state = self.lock();
        Ok(state.inodes[state.inode(path)?].live.clone())
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        Ok(self
            .lock()
            .live
            .keys()
            .filter(|path| path.parent() == Some(dir))
            .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
            .collect())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut state = self.lock();
        let id = state.inode(from)?;
        state.begin("rename")?;
        state.live.remove(from);
        state.live.insert(to.to_path_buf(), id);
        Ok(())
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        let mut state = self.lock();
        state.inode(path)?;
        state.begin("remove")?;
        state.live.remove(path);
        Ok(())
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        let mut state = self.lock();
        state.begin("sync directory")?;
        let in_dir = |path: &PathBuf| path.parent() == Some(dir);
        state.durable.retain(|path, _| !in_dir(path));
        let live: Vec<_> = state
            .live
            .iter()
            .filter(|(path, _)| in_dir(path))
            .map(|(path, id)| (path.clone(), *id))
            .collect();
        state.durable.extend(live);
        Ok(())
    }
}

impl FsFile for SimFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.fs.lock();
        state.begin("append")?;
        state.inodes[self.id].live.extend(bytes);
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let state = self.fs.lock();
        let live = &state.inodes[self.id].live;
        let bytes = usize::try_from(offset)
            .ok()
            .and_then(|start| live.get(start..start.checked_add(buf.len())?))
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        buf.copy_from_slice(bytes);
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        let mut state = self.fs.lock();
        state.begin("set length")?;
        let len = usize::try_from(len).map_err(io::Error::other)?;
        state.inodes[self.id].live.resize(len, 0);
        Ok(())
    }

    fn sync_data(&mut self) -> io::Result<()> {
        let mut state = self.fs.lock();
        state.begin("sync")?;
        let inode = &mut state.inodes[self.id];
        inode.durable.clone_from(&inode.live);
        Ok(())
    }

    fn len(&self) -> u64 {
        self.fs.lock().inodes[self.id].live.len() as u64
    }
}
