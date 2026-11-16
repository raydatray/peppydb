//! Raft for the peppydb server (plan 0028): openraft behind peppydb's own
//! types.
//!
//! This crate is internal to the server component. It owns everything that
//! names an openraft type, so `server` sees only peppydb types and an
//! openraft upgrade (plan 0025, decision 9) changes this crate alone.
//!
//! - [`Types`] is the openraft type configuration. It is generic over the
//!   async runtime: Tokio in production, the deterministic simulator in
//!   tests.
//! - [`Command`] is the replicated payload, with a stable binary codec.
//! - [`Transport`] is what a node needs from the network, over the peer
//!   messages in [`transport`]. `server` implements it (S7).

mod command;
#[cfg(any(test, feature = "sim"))]
pub mod rng;
#[cfg(any(test, feature = "sim"))]
pub mod sim;
pub mod transport;
mod types;

pub use command::CodecError;
pub use command::Command;
pub use command::Op;
pub use transport::Transport;
pub use types::LeaderId;
pub use types::Membership;
pub use types::NodeId;
pub use types::PeerInfo;
pub use types::Types;
