# Security issues inherited from Mostro

Open upstream issues in `MostroP2P/mostro` that the fork (`bipnipd`, v0.20.2) inherits, checked on
2026-11-10 (all OPEN). Rule: each is fixed, or gated so the affected path cannot run, **before any bipnip
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
