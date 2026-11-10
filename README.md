# bipnip

A Nostr network for Lightning node runners. bipnip is a fork of [Mostro](https://mostro.network) (the
Nostr-native peer-to-peer Lightning exchange) that keeps all of Mostro and adds:

- **Liquidity leases**: buy or sell an inbound channel of a set size and duration. The buyer keeps the
  payment preimage until its own node has verified the channel, so no instance or seller can take the
  payment without opening it.
- **Liquidity rings**: 2–5 node runners each open a channel to the next, with optional bonds.
- **Node-bound identities**: a Nostr key and a Lightning node key bind each other with signatures anyone
  can verify offline (`docs/nips/node-binding.md`).

Everything travels over Nostr relays. Anyone who runs a node can run an instance; instances coordinate
leases and rings but never hold lease money.

Status: **M0** (forks, specs, spikes). Nothing here is ready for real funds.

## Repositories

| repo | what | license |
|---|---|---|
| [`bipnip`](https://github.com/SovereignTechnology/bipnip) (this one) | `bipnip-agent` (runs next to your node, holds your keys), the `bipnip` CLI, `bipnip-ln` (LND, Core Lightning, LDK backends), later the web client; project docs | MIT OR Apache-2.0 |
| [`bipnipd`](https://github.com/SovereignTechnology/bipnipd) | the instance daemon, a fork of Mostro's `mostrod` | GPL-3.0-or-later |
| [`bipnip-core`](https://github.com/SovereignTechnology/bipnip-core) | protocol types: Mostro's, plus liquidity and node binding; a fork of `mostro-core` | MIT |
| [`bipnip-web`](https://github.com/SovereignTechnology/bipnip-web) | the web client: a fork of the [RoboSats](https://github.com/RoboSats/robosats) UI, served by your own agent | AGPL-3.0 |

Upstream Mostro apps keep working against a `bipnipd` instance for fiat trades.

## Docs

- Decisions: `docs/decisions/` (fork and licenses, roles, lease payment, node binding, event kinds,
  Lightning backends, rings, reputation, web-client security)
- Specs (drafts): `docs/nips/node-binding.md`, `docs/nips/liquidity-leases.md`,
  `docs/nips/liquidity-rings.md`
- `docs/executor-api.md`: how channel opens are handed to an executor (the node itself, or
  [vortex-ng](https://github.com/SovereignTechnology/vortex-ng) coinjoins)
- `docs/security/inherited-issues.md`, `docs/upstream-pins.md`

## Roadmap

M0 forks, specs and spikes · M1 Lightning backends (LND, CLN, LDK), node binding, the agent and CLI ·
M2 leases and rings · M3 vortex-ng as an executor · M4 discovery and reputation across instances ·
M5 the web client (the RoboSats UI fork: pure RoboSats first, then wired to bipnip; a Rust/WebAssembly
rewrite later) · M6 upstream contributions.

## Thanks

To the Mostro developers, whose daemon and protocol bipnip builds on, and to Amboss Magma,
LightningNetwork.plus and publsp for showing what node runners need.
