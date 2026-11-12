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
//!
//! The models are generic over the async runtime. Tests run them on
//! `openraft_rt_sim::SimRuntime`, so one seed fixes every timing.
//!
//! Built for this crate's tests and, with the `sim` feature, for the tests
//! of other server crates. Production code never enables the feature.

mod mem_log;
mod model_sm;
mod network;
mod router;
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
