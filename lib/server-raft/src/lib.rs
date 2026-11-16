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
//! - [`RaftNode`] is the facade: propose, read barrier, status, leader
//!   transfer, and the handlers for incoming peer messages.
//! - [`Transport`] is what the node needs from the network, over the
//!   peer messages in [`transport`]. `server` implements it (S7); tests
//!   use the simulator's network.

mod command;
mod error;
mod network;
mod node;
#[cfg(any(test, feature = "sim"))]
pub mod rng;
#[cfg(any(test, feature = "sim"))]
pub mod sim;
#[cfg(test)]
mod tests;
pub mod transport;
mod types;

pub use command::CodecError;
pub use command::Command;
pub use command::Op;
pub use error::LeaderHint;
pub use error::ProposeError;
pub use error::ReadError;
pub use error::StartError;
pub use error::StopError;
pub use node::HandleError;
pub use node::Proposed;
pub use node::RaftConfig;
pub use node::RaftNode;
pub use node::ReadPoint;
pub use node::Role;
pub use node::Status;
pub use transport::Transport;
pub use types::LeaderId;
pub use types::Membership;
pub use types::NodeId;
pub use types::PeerInfo;
pub use types::Types;
