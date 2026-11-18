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

## Notes from the LDK and Core Lightning implementations (2026-11-17)

Both were built on what real nodes answered (`spikes/ldk-m3/RESULTS.md`, `spikes/cln-m3/RESULTS.md`), not on
the plan above, which had guessed two things wrong.

- **A backend may lack a capability, and says so.** `BackendError::Unsupported` is an answer the callers
  handle: the conformance run goes on past it, and the agent refuses beforehand what its node cannot do (an
  agent on a node that cannot hold a payment offers no lease). Two capabilities may be missing: hold invoices
  and `graph_channels`. Everything else is required of every backend.
- **LDK is an LDK Server (`ldk-server`) over gRPC, not an embedded `ldk-node`**: the agent sits beside the
  runner's node, as with LND. It pays, opens channels and signs; **it offers no hold invoices**, so an LDK node
  buys leases and joins rings but does not sell. The server can make an invoice on a caller's hash, but its
  lock time is fixed (42 blocks, which left 6 blocks to claim), a held payment cannot be looked up (the only
  signal is a best-effort event; the payment list shows a payment the same way after the node gave it back),
  and an unpaid invoice cannot be cancelled. Offering that to a seller could cost it a channel opened for
  nothing. A lock time on `Bolt11ReceiveForHash` and a lookup of a held payment with its claim deadline would
  lift this.
- **Core Lightning is the node's own JSON-RPC socket, not `cln-grpc` with a rune.** Hold invoices come from the
  `hold` plugin (BoltzExchange/hold), whose commands are ordinary RPC methods on that socket; the other
  candidate (daywalker90/holdinvoice) cannot make an invoice on a hash the caller chose. The socket is the
  whole node: there is no narrower credential for a local client, and the agent must run as a user the node
  lets in. Through the plugin's RPC a hold invoice always locks for 80 blocks.
- **A node may limit the lock time of its hold invoices** (`max_hold_cltv_delta`). The seller asks for no more
  than its node gives, and an offer whose confirmation wait does not fit is refused when it is made.
  `add_hold_invoice` still refuses a request it cannot honour rather than shorten it.
- **Paying an invoice again is not an error on any backend**: while the payment is under way or once it is
  made, the answer is where that payment stands. LND and LDK refuse the repeat with an error of their own and
  Core Lightning refuses one and answers the other; each backend turns that into the same answer.
- **`graph_channels` exists only on LND.** LDK's and Core Lightning's graphs name a channel by short channel
  id, not by funding outpoint. Only an instance needs it (to judge a bond), and the instance runs on LND.
- **Nodes that follow the chain by polling lag a block for a moment** (Core Lightning, LDK Server). Callers
  wait for a count of confirmations; they do not assert it right after a block.


### Notes (2026-11-18): three lookups for watching a lease

- **A plain invoice has its own lookup (`invoice_state`)**, given the invoice's text: unpaid, paid, or
  expired. Before this, a seller and a bond claimant asked about their plain invoices through
  `hold_invoice_state`, which Core Lightning answers from the `hold` plugin only and LDK does not answer at
  all, so on those nodes a completion share or a bond claim could never be seen paid.
- **"No such payment" is an answer only from `payment_lookup`.** `payment_state` keeps refusing an unknown
  hash, and LND's refusals all look alike to a caller, so code that asks "did money already leave?" (the
  run's grant, a bond payment, a second completion invoice) asks `payment_lookup`, which says `None` only
  when the node says plainly it has no payment for that hash.
- **`closed_channel` tells a closing or closed channel apart from one never seen**, with the closer when
  the node knows it and the close height where it gives one (LND gives both; Core Lightning gives the
  closer and no height). LDK Server keeps no record of closed channels and answers `Unsupported`.
- The three are required of every backend (no default bodies), so a wrapper such as the agent's `Guard`
  cannot silently miss one.
