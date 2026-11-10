# 6. One Lightning backend trait for instance and agent

Date: 2026-11-10

## Status

Accepted (contract: `crates/bipnip-ln`)

## Context

`mostrod` speaks only to LND (fedimint-tonic-lnd 0.4). Its `EscrowBackend` trait (`src/escrow.rs`) covers
hold invoices with Mostro's own preimage; payouts, `channel_facts` and `get_node_info` call LND directly.
An LNbits request (#514) was closed. Hold-invoice subscriptions are lost when LND restarts (#1078).

bipnip needs LND, Core Lightning and LDK on both sides: the instance (fiat escrow, bonds, graph checks) and
the agent (hold invoices by external hash, opens, channel checks, `signmessage`).

## Decision

- `bipnip-ln::LightningBackend` (MIT OR Apache-2.0) is the one trait: node info, `sign_message`, hold
  invoices by a caller-chosen hash (add, state, settle, cancel), plain invoices, payments, connect,
  open, channels with a peer, graph lookups. Keys cross it as hex strings, so callers on either
  secp256k1 stack can use it.
- Implementations (M1): **LND** via fedimint-tonic-lnd 0.4 (the crate `mostrod` already uses) with a
  baked macaroon limited to the RPCs listed in the trait; **Core Lightning** via cln-grpc with a
  restricted rune, hold invoices through a plugin chosen in the M0 spike; **LDK** via ldk-node
  (`receive_for_hash` / `claim_for_hash` / `fail_for_hash`) or ldk-server's API, decided in the spike.
  Eclair later.
- `bipnipd` moves every direct LND call behind the trait; Mostro's escrow (own preimage) becomes "the
  instance generates P, then `add_hold_invoice(H)`", so one code path serves both.
- Subscriptions re-establish after a node restart and reconcile state by polling
  `hold_invoice_state` (fixes #1078 for every backend).

## Consequences

- The fork's largest upstream diff is this refactor; it is also the most useful piece to offer upstream.
- Each backend gets the same conformance tests in the regtest harness.
