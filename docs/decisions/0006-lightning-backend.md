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

## Notes from the LND implementation (2026-11-15)

- `pay_invoice` returns a `PaymentState` after a bounded wait and `payment_state(hash)` follows it: a payment
  to a hold invoice stays in flight until the payee settles, so "pay and return the preimage" cannot be the
  contract. A wait that runs out never cancels the payment.
- The LND backend is behind the `lnd` cargo feature because building `fedimint-tonic-lnd` needs `protoc`;
  the default workspace build (web host, offline builders) does not.
- Measured on LND v0.21.5-beta (`itest/ln`): settling an already-settled hold invoice again succeeds (a retry
  after a lost reply is safe); a preimage matching no invoice, and the right preimage for an unpaid invoice,
  are refused; a brand-new channel can fail the first payment (`failure reason 5`), so callers retry.
