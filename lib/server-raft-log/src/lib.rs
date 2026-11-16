//! The on-disk Raft log (plan 0029).
//!
//! Raft entries, the vote, the purge point, and the committed log id, kept
//! in segment files that survive process crashes and power loss. The log
//! knows nothing about openraft: entries are an index, a term, a kind, and
//! payload bytes. `server-raft` adapts it to openraft.

pub mod fs;
#[cfg(any(test, feature = "sim"))]
pub mod sim_fs;

#[cfg(test)]
mod tests;

pub use fs::{Fs, FsFile, RealFile, RealFs};
#[cfg(any(test, feature = "sim"))]
pub use sim_fs::{SimFile, SimFs, SimImage};
