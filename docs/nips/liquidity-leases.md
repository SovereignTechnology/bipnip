Liquidity Leases over Mostro instances
======================================

`draft` · bipnip, 2026-11-10 · requires: the node-binding draft, Mostro protocol v2

A seller opens a Lightning channel of agreed capacity to a buyer and keeps it open for an agreed number of
blocks; the buyer pays a fee. An instance (a `bipnipd`) coordinates, but never holds the payment or its
preimage: **the buyer keeps the preimage until its own node has verified the channel**.

## Roles and identities

- **Seller** and **buyer**: agents with a node and a node binding (kind 31735). The identity they prove
  in Mostro's transport for a lease is their node-bound key.
- **Instance**: publishes the order, checks bindings and rules against its own node's graph, relays
  messages, records ratings, and holds the optional buyer bond.

## Lease order (kind 38735, `k` = `lease`)

Published and signed by the instance after a seller's `NewOrder`, `d` = order id. Tags (one value each
unless noted):

| tag | meaning |
|---|---|
| `k` | `lease` |
| `s` | status: `pending`, `in-progress`, `success`, `canceled`, `expired` |
| `node` | the seller's node key (66 hex); the seller's binding must verify |
| `network` | as in the binding |
| `capacity` | `<min_sat>` `<max_sat>` (two values) |
| `duration` | lease length in blocks |
| `fee` | `<base_sat>` `<ppm of capacity>` (two values) |
| `split` | upfront share in percent (1–100); the rest is paid at lease end |
| `fee_caps` | `<max base msat>` `<max ppm>` the seller promises not to exceed on this channel |
| `confs` | confirmations the buyer waits for before releasing the preimage |
| `announce` | `true` or `false` |
| `channel_type` | `anchors` or `taproot` (repeatable) |
| `rules` | buyer requirements, `key=value` values (e.g. `min_channels=3`, `min_capacity=5000000`, `clearnet=true`) |
| `bond` | buyer bond in sat, or `0` |
| `expiration` | NIP-40 |

## Flow

All messages are Mostro protocol v2 messages (kind 14, NIP-44) unless marked *peer*, which go directly
between the two trade keys over the same transport.

1. **Take.** The buyer's agent generates a random 32-byte preimage `P`, computes `H = sha256(P)` and sends
   `TakeLease` with: order id, capacity, its binding event, its node address, and `H`. The instance checks
   the binding, the rules against its own graph, and the capacity range; if `bond > 0` it asks the buyer
   to pay a bond hold invoice (Mostro's bond flow).
2. **Invoice.** The instance forwards the take to the seller. The seller's agent has its **own node**
   create a hold invoice on `H` for the upfront share, valid ≤ 48 h, `min_final_cltv_expiry_delta` within
   the instance's `payment_cltv_limit`, and sends it (`LeaseInvoice`).
3. **Pay.** The buyer's agent checks: payee = the seller's bound node key, payment hash = `H`, amount =
   the upfront share for the chosen capacity, expiry and CLTV within limits. It pays; the HTLC is held at
   the seller's node.
4. **Open.** When its node reports the invoice accepted, the seller opens the channel to the buyer
   (from its wallet or as a vortex-ng coinjoin output) and sends `LeaseChannelOpened` with the funding
   outpoint.
5. **Verify and release** (*peer*). The buyer's agent waits until its own node shows a channel with:
   remote = the seller's node, the agreed capacity, seller as initiator, the agreed channel type and
   announce flag, at least `confs` confirmations. Then it sends `P` to the seller's trade key
   (`LeasePreimage`). Before this point `P` is never sent anywhere.
6. **Settle.** The seller's agent settles the hold invoice with `P` and reports `LeaseActive`.
7. **Completion.** At `funding height + duration`, the seller sends an ordinary invoice for the remaining
   share (`LeaseCompletionInvoice`); the buyer's agent pays it if its node still sees the channel open.
8. **Outcomes.** Each party may publish a kind 8735 outcome; both may rate through Mostro's rating flow.

## Failure handling

| case | result |
|---|---|
| seller never opens | the seller cancels, or the HTLC times out; the buyer is refunded; negative outcome |
| channel differs from the terms | the buyer does not release `P`; the invoice expires; dispute if the seller claims otherwise |
| buyer withholds `P` after a verified open | announced channel visible in the instance's graph → the buyer bond is slashed to the seller; unannounced → solver decides from the signed messages |
| seller closes early | completion share unpaid; negative outcome |
| seller raises fees above `fee_caps` | negative outcome; buyer may close |

## Limits

- Hold windows ≤ 48 h: held HTLCs tie up liquidity on every node in the payment path.
- A seller's node must support hold invoices on an external hash (LND `AddHoldInvoice`, Core Lightning with
  a hold-invoice plugin, LDK `receive_for_hash`).
