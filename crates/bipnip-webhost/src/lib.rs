//! `bipnip-webhost`: serves the bipnip-web UI (the RoboSats UI fork, AGPL-3.0, read from disk, not
//! compiled in) and proxies coordinators over Tor, so the page only ever talks to its own origin
//! (W0 of the web client plan; `docs/decisions/0009-web-client-security.md`). With an agent's web
//! door named, it also passes a paired page's signed requests to the agent (W1;
//! `docs/decisions/0011-key-roles-pairing-and-session-keys.md`).

pub mod agent;
pub mod app;
pub mod connector;
pub mod federation;
pub mod html;
pub mod security;
