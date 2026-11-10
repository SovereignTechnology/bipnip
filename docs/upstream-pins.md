# Upstream pins

bipnip forks Mostro and follows these revisions. Moving a pin is deliberate: merge the upstream release on
a branch, read its diff (protocol, escrow, migrations), resolve conflicts, run upstream's and bipnip's
tests, and record it below.

| what | revision | date | why |
|---|---|---|---|
| `MostroP2P/mostro` release `v0.20.2` (tag object `5e123b0b`) | commit `fe2a9052808157b038ae4800a7b1d3785810f277` | 2026-10-28 | fork point of `bipnipd` (GPL-3.0-or-later since v0.19.2) |
| `MostroP2P/mostro-core` release `v0.17.3` (tag object `a85ca6a3`) | commit `97a1fa84fa81d92368c852b943eabf3711564aa2` | 2026-10-07 | fork point of `bipnip-core` (MIT) |
| Mostro protocol | transport v2 (kind 14 + NIP-44), kinds 38383–38389, 8383 | as of v0.20.2 | wire compatibility with upstream apps |
| `nostr-protocol/nips` master | `b587c619c2ea662d40b40976aeccf6ac8d6ba9eb` | 2026-11-10 | kind registry checked for 31735, 38735, 8735 |
| `smallworlnd/publsp` HEAD | `07d5891aef9c4e6e4f44aaff24cf453b69baafa2` | 2026-11-10 | its kind 39735 offers, read for ADR 0005 |
| `lightningdevkit/rust-lightning` main, `lightning/src/util/message_signing.rs` | read 2026-11-10 | 2026-11-10 | message-signing test vectors in `bipnip-core` |

## Lightning implementations

| implementation | version | used for |
|---|---|---|
| LND | 0.21.x (booty's signet lab runs 0.21.5-beta) | hold invoices by hash, opens, `signmessage`, the instance's escrow |
| Core Lightning | 26.06.x (eltor-lab regtest) | same, hold invoices through a plugin (M0 spike picks it) |
| LDK | ldk-node 0.7.x / ldk-server (LDK 0.3.0-rc3 in booty's lab) | same, `receive_for_hash` |
| Eclair | 0.14.x | later; `signmessage` returns hex |

## Toolchain

Rust 1.94.0 (`rust-toolchain.toml`), upstream Mostro's pin, in all three repos.

## History

- 2026-11-10: initial pins (M0).
