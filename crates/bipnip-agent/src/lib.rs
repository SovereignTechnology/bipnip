//! The bipnip agent's library: what the daemon does, testable without a node
//! (`docs/decisions/0002-instance-and-agent-roles.md`).

pub mod binding;
pub mod book;
pub mod config;
pub mod daemon;
pub mod keys;
pub mod lease;
pub mod lease_wire;
pub mod lock;
pub mod record;
pub mod relay;
pub mod ring;
pub mod ring_wire;
pub mod secretfile;
