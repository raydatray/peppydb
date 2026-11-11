//! Test models and the deterministic simulator (plan 0028).
//!
//! Each model keeps the contract of a real component that a later stack
//! builds, so the simulator tests that contract before the component
//! exists:
//!
//! - [`MemLog`] stands in for the on-disk Raft log (plan 0025, S5).
//! - [`ModelSm`] stands in for `engine::Db` in `Mode::External` behind
//!   openraft (plan 0025, S6).
//! - [`Router`] is a perfect in-memory transport, and [`SimNetwork`] adds
//!   faults to it.
//! - `simulator` (tests only) runs a seeded cluster under faults.
//!
//! The models are generic over the async runtime. Tests run them on
//! `openraft_rt_sim::SimRuntime`, so one seed fixes every timing.
//!
//! Built for this crate's tests and, with the `sim` feature, for the tests
//! of other server crates. Production code never enables the feature.

#[cfg(test)]
pub mod checker;
mod mem_log;
mod model_sm;
mod network;
mod router;
#[cfg(test)]
pub mod simulator;
mod trace;

pub use mem_log::LogConfig;
pub use mem_log::LogImage;
pub use mem_log::MemLog;
pub use model_sm::APPLIED_KEY;
pub use model_sm::Contents;
pub use model_sm::MEMBERSHIP_KEY;
pub use model_sm::ModelSm;
pub use model_sm::StoredSnapshot;
pub use model_sm::SyncHook;
pub use network::Faults;
pub use network::NetStats;
pub use network::SimNetwork;
pub use router::Router;
pub use trace::Trace;

use std::sync::Arc;

use crate::transport::AppendReply;
use crate::transport::Vote;
use crate::transport::VoteReply;
use crate::transport::VoteRequest;
use crate::types::NodeId;
use crate::types::OrEntry;
use crate::types::OrVote;

/// Receives what the storage models do, for the safety checks.
pub(crate) trait Observer: Send + Sync {
    /// `node` made `vote` durable; `live` is its log at that moment.
    fn vote_saved(&self, node: NodeId, vote: OrVote, live: &LogImage);
    /// `node`'s state machine applied `entry` (redeliveries are skipped
    /// and not reported).
    fn applied(&self, node: NodeId, entry: &OrEntry);
}

/// A model's observer and the node it reports as.
pub(crate) type Observed = Option<(NodeId, Arc<dyn Observer>)>;

/// Sees messages as the simulated network delivers them, for the
/// persist-before-send check.
pub trait Probe: Send + Sync {
    /// `from` sends a message that carries its `vote`.
    fn request(&self, from: NodeId, kind: &str, vote: &Vote);
    /// `to` answers a vote request.
    fn vote_reply(&self, to: NodeId, req: &VoteRequest, reply: &VoteReply);
    /// `to` answers an append request.
    fn append_reply(&self, to: NodeId, reply: &AppendReply);
}
