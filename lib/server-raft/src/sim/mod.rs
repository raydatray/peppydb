//! Test models and the deterministic simulator (plan 0028).
//!
//! Each model keeps the contract of a real component that a later stack
//! builds, so the simulator tests that contract before the component
//! exists:
//!
//! - [`MemLog`] stands in for the on-disk Raft log (plan 0025, S5).
//!
//! The models are generic over the async runtime. Tests run them on
//! `openraft_rt_sim::SimRuntime`, so one seed fixes every timing.
//!
//! Built for this crate's tests and, with the `sim` feature, for the tests
//! of other server crates. Production code never enables the feature.

mod mem_log;

pub use mem_log::LogConfig;
pub use mem_log::LogImage;
pub use mem_log::MemLog;
