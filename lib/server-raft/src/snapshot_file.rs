//! File-backed snapshot storage for `DbStateMachine` (plan 0034).
//!
//! Production state machines stream `Db::write_snapshot` into a temporary file
//! under `<data_dir>/snapshots/` and stream `Db::install_snapshot` directly
//! from disk, keeping memory usage `O(chunk_size)` regardless of database size.
//! In-memory tests and the deterministic simulator use [`SnapshotData::Bytes`].

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// A temporary snapshot file on disk with its exact byte length and Castagnoli
/// CRC32C checksum. The file is automatically unlinked when dropped.
pub struct SnapshotFile {
    path: PathBuf,
    len: u64,
    crc32c: u32,
}

impl fmt::Debug for SnapshotFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotFile")
            .field("path", &self.path)
            .field("len", &self.len)
            .field("crc32c", &format_args!("{:#010x}", self.crc32c))
            .finish()
    }
}

impl Drop for SnapshotFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Counting + CRC32C-hashing writer around a buffered file.
struct CrcWriter<W> {
    inner: W,
    len: u64,
    crc: u32,
}

impl<W: Write> CrcWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            len: 0,
            crc: 0,
        }
    }
}

impl<W: Write> Write for CrcWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.len = self.len.saturating_add(n as u64);
        self.crc = crc32c::crc32c_append(self.crc, &buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl SnapshotFile {
    /// Ensure `dir` exists and remove any leftover `*.tmp` or `*.part` files
    /// from an earlier crashed process.
    pub fn clean_stale(dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if let Some(ext) = path.extension().and_then(|e| e.to_str())
                && (ext == "tmp" || ext == "part")
            {
                let _ = fs::remove_file(&path);
            }
        }
        Ok(())
    }

    /// Create a temporary snapshot file in `dir` by streaming through `write_fn`,
    /// computing total length and Castagnoli CRC32C in one pass.
    pub fn create_in(
        dir: &Path,
        write_fn: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = dir.join(format!("snap-{pid}-{id}.tmp"));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        let mut writer = CrcWriter::new(BufWriter::new(file));
        if let Err(err) = write_fn(&mut writer).and_then(|()| writer.flush()) {
            drop(writer);
            let _ = fs::remove_file(&path);
            return Err(err);
        }
        let len = writer.len;
        let crc32c = writer.crc;
        drop(writer);
        Ok(Self { path, len, crc32c })
    }

    /// Take ownership of an already-staged snapshot file whose length and
    /// CRC32C have been verified. The file will be unlinked on drop.
    pub fn from_existing(path: PathBuf, len: u64, crc32c: u32) -> Self {
        Self { path, len, crc32c }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn crc32c(&self) -> u32 {
        self.crc32c
    }

    /// Open a read-only handle seeked to `offset`.
    pub fn open_at(&self, offset: u64) -> io::Result<File> {
        let mut file = File::open(&self.path)?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))?;
        }
        Ok(file)
    }
}

/// Snapshot payload carried between `DbStateMachine` and `Transport`.
pub enum SnapshotData {
    /// In-memory bytes, used by in-memory tests and the simulator.
    Bytes(Vec<u8>),
    /// Temporary file on disk, used by production `DbStateMachine`.
    File(SnapshotFile),
}

impl fmt::Debug for SnapshotData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes(b) => f
                .debug_struct("SnapshotData::Bytes")
                .field("len", &b.len())
                .finish(),
            Self::File(file) => f.debug_tuple("SnapshotData::File").field(file).finish(),
        }
    }
}

impl SnapshotData {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::Bytes(bytes)
    }

    pub fn len(&self) -> u64 {
        match self {
            Self::Bytes(b) => b.len() as u64,
            Self::File(f) => f.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Castagnoli CRC32C over the complete snapshot payload.
    pub fn crc32c(&self) -> u32 {
        match self {
            Self::Bytes(b) => crc32c::crc32c(b),
            Self::File(f) => f.crc32c(),
        }
    }

    /// Open a buffered reader positioned at `offset`.
    pub fn open_reader_at(&self, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        match self {
            Self::Bytes(b) => {
                let start = usize::try_from(offset)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;
                if start > b.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "offset past end of snapshot",
                    ));
                }
                Ok(Box::new(Cursor::new(b[start..].to_vec())))
            }
            Self::File(f) => {
                if offset > f.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "offset past end of snapshot",
                    ));
                }
                Ok(Box::new(BufReader::new(f.open_at(offset)?)))
            }
        }
    }

    /// Consume the snapshot and read all bytes into memory (used by the in-memory simulator).
    #[cfg(test)]
    pub(crate) fn into_bytes(self) -> io::Result<Vec<u8>> {
        match self {
            Self::Bytes(b) => Ok(b),
            Self::File(f) => fs::read(f.path()),
        }
    }

    /// Read the complete snapshot into memory (for tests).
    #[cfg(test)]
    pub(crate) fn to_bytes(&self) -> io::Result<Vec<u8>> {
        match self {
            Self::Bytes(b) => Ok(b.clone()),
            Self::File(f) => fs::read(f.path()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_file_computes_crc_seeks_and_unlinks_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"hello file-backed snapshot payload!";
        let file = SnapshotFile::create_in(dir.path(), |w| w.write_all(payload)).unwrap();
        let path = file.path().to_path_buf();
        assert!(path.exists());
        assert_eq!(file.len(), payload.len() as u64);
        assert_eq!(file.crc32c(), crc32c::crc32c(payload));

        let mut tail = Vec::new();
        file.open_at(6).unwrap().read_to_end(&mut tail).unwrap();
        assert_eq!(tail, &payload[6..]);

        drop(file);
        assert!(!path.exists(), "temp file unlinked on drop");
    }

    #[test]
    fn clean_stale_removes_leftover_tmp_and_part_files() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("snap-1-1.tmp");
        let part = dir.path().join("recv-1-10.part");
        let keep = dir.path().join("README.txt");
        fs::write(&tmp, b"stale").unwrap();
        fs::write(&part, b"partial").unwrap();
        fs::write(&keep, b"keep").unwrap();

        SnapshotFile::clean_stale(dir.path()).unwrap();
        assert!(!tmp.exists());
        assert!(!part.exists());
        assert!(keep.exists());
    }
}
