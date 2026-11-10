# 1. Fork Mostro, keep all of it, track its releases

Date: 2026-11-10

## Status

Accepted

## Context

bipnip is a Nostr network for Lightning node runners: buy and sell inbound channels (as Amboss Magma
does), form liquidity rings (as LightningNetwork.plus does), with every identity bound to a Lightning node
key. The project owner chose to fork Mostro and keep all of its features ("fork Mostro and use it", "ya we
are taking all the features"): peer-to-peer fiat trading with hold-invoice escrow, bonds, disputes and
solvers, ratings, range orders, cooperative cancel, session restore and the Cashu escrow mode.

Facts checked 2026-11-10:
- `MostroP2P/mostro` (the daemon, `mostrod`) is GPL-3.0-or-later since v0.19.2 (PR #957, merged
  2026-10-03); v0.19.1 is its last MIT release. Latest release v0.20.2 (2026-10-28).
- `MostroP2P/mostro-core` (protocol types and the NIP-44 transport) is MIT; a GPL relicense PR (#167)
  was closed unmerged. Latest release v0.17.3 (2026-10-07).
- `mostro-cli` and `mostrix` are GPL-3.0-only; the v2 app (Flutter, also the web build) is AGPL-3.0.
- Upstream moves fast (~100 commits a month, about weekly releases) and has broken its wire protocol
  before (transport v1 → v2 in v0.19.0).

## Decision

- **Three repositories:**

  | repo | role | base | license |
  |---|---|---|---|
  | `bipnip-core` | protocol types: Mostro's unchanged, plus liquidity and node binding | fork of mostro-core at v0.17.3, history kept | MIT |
  | `bipnipd` | the instance daemon | fork of mostro at v0.20.2, history kept | GPL-3.0-or-later |
  | `bipnip` | agent, CLI, Lightning backends, later the web client | new | MIT OR Apache-2.0 |

- **Fork the current release (GPL), not v0.19.1 (MIT):** all features and future upstream fixes can be
  merged. Code bipnip writes outside the daemon (`bipnip-core` additions, everything in `bipnip`) keeps a
  permissive license so other projects, including upstream Mostro, can take it.
- **Track upstream releases and stay wire-compatible.** Each upstream release is merged into the forks
  on a branch with a merge commit (not squashed, so history stays traceable). Mostro's message types,
  serialization, event kinds and tags stay byte-identical, so upstream Mostro apps (app, mostrix,
  mostro-cli) keep trading fiat on a `bipnipd` instance. Liquidity uses new kinds and new message
  variants that upstream apps never receive.
- **Keep merges cheap:** bipnip code lives in new files and new tables; edits to upstream files are kept
  to wiring lines (a module declaration, a match arm, a dependency feature). Package and target names stay
  `mostro`, `mostrod` and `mostro-core` inside the forks (dependents point at the fork by git), so
  upstream's version bumps, Dockerfiles and install docs never conflict; bipnip's own packaging
  (systemd unit, release archive) installs the daemon as `bipnipd`.
- **Nothing from Mostro's apps is forked.** Their licenses (GPL-only, AGPL) and the web app's key
  storage (the seed reachable from page script) conflict with bipnip's agent model (ADR 0009).
- **Upstream contributions** (the backend trait, proven node identity in kind 38385, security fixes) are
  offered as PRs after the duplicate check and after the project owner reads them; upstream's
  `AGENTS.md` requires an issue labelled `status: accepted` before any PR, and manual testing notes.
- **Toolchain:** Rust 1.94.0 in all three repos (upstream's pin), so `bipnip-ln` builds inside
  `bipnipd`'s workspace.

## Consequences

- Every upstream release merge is a scheduled task with conflicts to resolve and the full upstream test
  suite plus bipnip's tests to pass before the merge lands.
- The daemon cannot be relicensed; anything copied from it into `bipnip` would make that code GPL, so
  `bipnip` never copies daemon code (it may use `bipnip-core`, which is MIT).
- Mostro's open security issues come with the fork (docs/security/inherited-issues.md) and are fixed or
  gated before any bipnip money path ships.
