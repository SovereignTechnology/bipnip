# 3. Lease payment: the buyer keeps the preimage

Date: 2026-11-10

## Status

Accepted (normative text: `docs/nips/liquidity-leases.md`)

## Context

A lease: a seller opens a channel of agreed capacity to the buyer and keeps it open for an agreed number
of blocks; the buyer pays a fee. Existing markets (checked 2026-11-10):
- **Magma** (closed): the seller's hold invoice (valid ≥ 48 h) is paid by the buyer and settled once Magma
  verifies the channel; keeping it open afterwards is enforced only by monitoring and reputation.
- **publsp** (MIT) and bLIP-51: the LSP creates the hold invoice and holds its preimage; the buyer trusts
  the LSP to open before settling.
- **Mostro**: the instance creates every hold invoice and stores its preimage.
- **BOLT liquidity ads** (#1153, still open) drop the CSV lock that would have enforced duration; no
  deployed protocol enforces a lease after the open.

A hold invoice protects whoever holds the preimage. In every design above that is the party paid, or a
middleman. The buyer, the party at risk, holds nothing.

## Decision

- **The buyer's agent generates the preimage P and sends only its hash H.** The **seller's own node**
  creates a hold invoice on H for the upfront share. The buyer's agent checks the invoice (payee = the
  seller's bound node key, hash = H, amount, expiry and CLTV within limits) and pays it. The HTLC is
  locked at the seller's node; nobody can settle without P.
- The seller opens the channel (from its node's wallet, or as a vortex-ng coinjoin output) and reports
  the funding outpoint. The buyer's agent verifies the channel **on its own node**: remote = the seller,
  capacity, seller-initiated, channel type, announce flag, the agreed confirmations. Only then does it
  send P to the seller (directly to the seller's trade key). The seller settles.
- If the seller never opens, the HTLC is cancelled or times out and the buyer is refunded. The instance
  never sees P before release and never holds lease funds.
- **Duration** is not enforceable by protocol today, so the fee is split: an upfront share (above) and a
  completion share, an ordinary invoice the seller issues at lease end and the buyer pays if its node
  still sees the channel. An early close forfeits the completion share and records a negative outcome.
- **Buyer griefing** (the buyer withholds P after a verified open): an optional buyer bond, using
  Mostro's bond feature and held by the instance for the open window only, is slashed to the seller when
  the instance's own graph shows the announced channel and P was not released in time; for unannounced
  channels the case goes to a solver with both signed contract messages as evidence.
- **Bounds:** hold windows ≤ 48 h and CLTV ≤ the instance's `payment_cltv_limit` (Mostro default 1008),
  because held HTLCs tie up liquidity on every routing node in the path.

## Consequences

- The seller takes the first on-chain cost (opening before being paid). This is the same exposure a
  Magma or LN+ seller has today, and the bond covers the griefing case where it is provable.
- The instance's role in a lease is message passing, checks against its graph, ratings and, if asked, a
  bond: an instance cannot steal a lease payment.
- A seller's node must support hold invoices by external hash (LND, CLN with a plugin, LDK; checked in
  the M0 spikes).
