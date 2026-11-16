//! The bipnip agent's library: what the daemon does, testable without a node
//! (`docs/decisions/0002-instance-and-agent-roles.md`).

pub mod binding;
pub mod config;
pub mod keys;
pub mod relay;
pub mod secretfile;
