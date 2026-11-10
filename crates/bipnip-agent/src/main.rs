//! `bipnip-agent`: the daemon a node runner runs next to their Lightning node (built in M1).
//!
//! It holds the Mostro-style NIP-06 mnemonic and the node credentials, acts as a Mostro client for
//! fiat trades and as buyer, seller or ring member for liquidity, and is the security boundary for
//! every client (`docs/decisions/0002-instance-and-agent-roles.md`,
//! `docs/decisions/0009-web-client-security.md`).

fn main() {
    let _contract: Option<&dyn bipnip_ln::LightningBackend> = None;
    eprintln!("bipnip-agent: not built yet (milestone M1); see docs/decisions/");
    std::process::exit(2);
}
