//! `bipnip-webhost`: serves the bipnip-web UI (the RoboSats UI fork, AGPL-3.0, read from disk, not
//! compiled in) and proxies coordinators over Tor, so the page only ever talks to its own origin
//! (W0 of the web client plan; `docs/decisions/0009-web-client-security.md`).

pub mod app;
pub mod connector;
pub mod federation;
pub mod html;
pub mod security;
