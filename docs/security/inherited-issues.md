# Security issues inherited from Mostro

Open upstream issues in `MostroP2P/mostro` that the fork (`bipnipd`, v0.20.2) inherits, checked on
2026-11-15 (all OPEN). Rule: each is fixed, or gated so the affected path cannot run, **before any bipnip
money path ships** (M2). A fix is first looked for upstream (merged → take it on the next release merge);
otherwise bipnip fixes it with a regression test first, and offers it upstream after the duplicate check
and after the project owner reads it.

| # | severity | issue | bipnip plan |
|---|---|---|---|
| 807 | HIGH | Admin gRPC has no code-level authentication (fix PR #877 open) | **Gated:** `[rpc] enabled = false` by default, bound to 127.0.0.1; v0.20.2 already accepts an optional `auth_token` for the mutating RPCs. bipnipd refuses to start with RPC enabled and no `auth_token` (M1). Follow #877; don't duplicate it. |
| 1102 | — | The daemon key acts as a dispute solver over Nostr, bypassing the solver checks and the takeover rule | Fix in M1: the daemon key is never a solver; admin actions from it are refused unless they come through the authenticated RPC. |
| 808 | HIGH | Swallowed order-status DB writes cause DB↔Nostr divergence (take_sell/flow) | Fix in M1: a failed write aborts the action before anything is published; regression test. |
| 904 | MEDIUM | `release_action` settles the hold invoice before its CAS guard | Fix in M1: guard (compare-and-set on status) before settle; regression test. Applies to bond and lease settles too. |
| 836 | — | Nostr keys logged in cleartext across modules | Fix in M1 structurally: key types wrap secrets (`secrecy`), `Debug`/`Display` redact; a test greps logs for key shapes. |
| 1078 | — | Hold-invoice subscriptions lost when LND restarts | Fixed by design in `bipnip-ln` (ADR 0006): re-subscribe and reconcile by polling state, every backend. |
| 1014 | — | LND credential and settings file permissions | Fix in M1: refuse to start when the settings file, macaroon, rune or database is group/world-readable; create files 0600. |
| 954 | — | `SQLITE_BUSY` under concurrent writes drops user actions | Fix in M1: WAL + busy timeout + retry of the whole action; a concurrency regression test. |

Also, by design rather than a filed issue: escrow preimages (fiat trades) sit in SQLite in cleartext.
bipnipd keeps the database 0600 and adds encryption at rest for the preimage column (M1). Lease and ring
preimages never reach the instance (ADR 0003).

## Status on 2026-11-25 (final for H1)

bipnipd main (76f3213) has Mostro v0.20.3 merged (bipnipd #13) and every fix below. All eight issues are
still open upstream.

| # | where it stands |
|---|---|
| 807 | **Gated only, and left to upstream** (the owner, 2026-11-25: wait for upstream). The RPC is off by default and binds to 127.0.0.1, and v0.20.3 checks an optional `auth_token` on the mutating RPCs. The refusal to start with the RPC on and no token, planned for M1 above, was **never built**: an operator who turns the RPC on without a token gets unauthenticated `CancelOrder`, `SettleOrder`, `AddSolver` and `TakeDispute` for any peer that reaches the port. Upstream's mandatory-token PR #877 is stalled (the maintainer said on 2026-11-23 it cannot land as it is). Tracked as bipnipd #19. |
| 1102 | **Fixed**, bipnipd #14: both event loops refuse the daemon key as a solver; the RPC keeps it. |
| 808 | **Audited** 2026-11-24: the sites the issue names now use compare-and-swap, and no write that precedes a publish is left with its failure swallowed (the dispute auto-close publishes only after a successful write; the Cashu path is off). Nothing to change. |
| 904 | **Fixed**, bipnipd #2: the release is claimed by compare-and-swap before the hold invoice is settled. A second concurrent release is told `Ok` or, when it reads the order after the claim, `NotAllowedByStatus`; either way nothing settles twice. |
| 836 | **Left to upstream:** PR #842 (open) drops the log lines that pair two user keys; v0.20.3 stopped logging DM payloads and receiver keys (#1123). Take #842 when it merges. |
| 1078 | **Fixed**, bipnipd #17 (`keep_watching`: subscribe again when a stream ends, until the invoice is final or nothing waits on it). |
| 1014 | **Fixed**, fork-only, bipnipd #16: no start while the settings, `.env`, macaroon or database can be read by another user, or the settings directory written by one. |
| 954 | **Fixed**, bipnipd #3. |
| preimages at rest | **Sealed**, bipnipd #18 (NIP-44 to the daemon's own key, bound to the row; fiat escrow, anti-abuse and liquidity bonds; cleartext sealed at startup). |

The fiat flow is checked end to end with upstream's own `mostro-cli` (`itest/ln/run-fiat.sh`), and the
week-long soak (`itest/soak/`, on booty) runs the merged bipnipd.

## Status on 2026-11-15

Checked with the duplicate guard before any work (`guard-pr-duplicate.py MostroP2P/mostro N`):

- **Claimed upstream, not duplicated here:** #807 by PR #877 (open, stale since 2026-09-03, conflicting,
  changes requested; it makes an enabled RPC without a token fatal) and #1014 by PR #890 (open). bipnipd keeps
  the RPC off by default until one of them lands or the project owner decides to carry a fork-only gate.
- **#836:** not claimed, but PR #842 (open) touches it; read it before starting.
- **Free:** #1102, #808, #904, #1078, #954.
- **#904:** fix on bipnipd branch `fix/904-release-claim-before-settle` (regression test first, failing on the
  old order with 2 settles; then claim-before-settle). One deliberate difference from the issue text: a failed
  settle hands the claim back by compare-and-swap so the seller can retry, since no path recovers an order
  left in `settled-hold-invoice` with an unsettled invoice. Money path: waits for the project owner's read.
- **#1078:** `bipnip-ln`'s LND backend reads hold-invoice state by lookup, with no subscription to lose
  (conformance run in `itest/ln`); it takes effect in bipnipd when the daemon moves onto the backend.
