//! The agent as a daemon (`bipnip-agent run`): one process that holds the wallet and the node,
//! carries every stored lease and ring run on, and answers requests on a unix socket
//! (`docs/decisions/0010-agent-daemon-and-local-api.md`, `docs/agent-api.md`).
//!
//! - [`api`]: the words of the socket: requests, answers, error codes. Pure.
//! - [`policy`]: the limits, the record of what was granted and parked, and the guard between
//!   a run and the node.
//! - [`engine`]: the one worker that steps the runs and answers the requests.
//! - [`reads`]: the requests that only read the relays, answered beside the engine.
//! - [`server`]: the socket, and the loop that feeds the engine.

pub mod api;
pub mod engine;
pub mod policy;
pub mod reads;
pub mod server;

#[cfg(test)]
mod tests;
