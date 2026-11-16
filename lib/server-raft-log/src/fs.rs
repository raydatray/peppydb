//! What the log needs from a file system (plan 0029, "The file-system
//! trait").
//!
//! [`RealFs`] calls `std::fs`. `SimFs` (feature `sim`) keeps files in
//! memory with a power-loss model, so crash tests can stop at every
//! operation and the deterministic simulator can run the real log.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// A file system. Paths name files directly inside a log directory.
pub trait Fs: Clone + Send + Sync + 'static {
    type File: FsFile;
    /// Create `dir` and its parents if they are absent.
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
    /// Create a new, empty file; fail if `path` exists.
    fn create_new(&self, path: &Path) -> io::Result<Self::File>;
    /// Open an existing file for reading and appending.
    fn open(&self, path: &Path) -> io::Result<Self::File>;
    /// The whole contents of a file.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// The names of the files in `dir`. An absent directory is empty.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;
    /// Replace `to` with `from`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove(&self, path: &Path) -> io::Result<()>;
    /// Make the names created, renamed, and removed in `dir` durable.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
}

/// An open file. Writes only append; `set_len` shortens.
pub trait FsFile: Send + 'static {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Fill `buf` from `offset`; fail if the file is shorter.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    /// Make the contents and length durable.
    fn sync_data(&mut self) -> io::Result<()>;
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The operating system's file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct RealFs;

/// A file of [`RealFs`], opened for appending.
#[derive(Debug)]
pub struct RealFile {
    file: File,
    len: u64,
}

impl RealFile {
    fn new(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}

impl Fs for RealFs {
    type File = RealFile;

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    fn create_new(&self, path: &Path) -> io::Result<RealFile> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(path)?;
        RealFile::new(file)
    }

    fn open(&self, path: &Path) -> io::Result<RealFile> {
        RealFile::new(OpenOptions::new().read(true).append(true).open(path)?)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut names = Vec::new();
        for entry in entries {
            if let Ok(name) = entry?.file_name().into_string() {
                names.push(name);
            }
        }
        Ok(names)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        wal_segments::sync_dir(dir)
    }
}

impl FsFile for RealFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)?;
        self.len += bytes.len() as u64;
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)?;
        self.len = len;
        Ok(())
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn len(&self) -> u64 {
        self.len
    }
}
