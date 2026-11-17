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

Published and signed by the instance after a seller's `lease-offer`, `d` = order id. Tags (one value each
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

Implemented so far (`bipnip-core`, `lease`, `lease_message` and `lease_order`): `node`, `network`,
`capacity`, `duration`, `fee`, `split`, `confs`, `announce`, `bond` and the expiry. `fee_caps`,
`channel_type` and `rules` are not part of the v1 contract yet. The buyer's bond works as described under
[The buyer's bond](#the-buyers-bond): the `bond` tag and the offer's `bond_sat` (0, or at least 1 000 sat,
at most the smallest capacity on offer, announced channels only). The bond is not in the contract text:
the buyer agrees to it by paying the instance's bond invoice.

### Status

| `s` | when |
|---|---|
| `pending` | the offer is open |
| `in-progress` | a buyer took it; the lease is being set up |
| `success` | the seller reported the lease active |
| `canceled` | withdrawn by the seller, canceled by either party before the lease was active, or stalled |
| `expired` | nobody took it in time |

- The instance republishes the order when its status changes, each event dated after the last, so a relay
  always keeps the newest.
- `expiration` is the offer's expiry, whatever the status. Relays drop the event then, so the instance
  publishes nothing for the order after that point; in practice `expired` is implied by the event
  disappearing, and a client must treat a `pending` order past its `expiration` as closed.
- An order that is canceled before its first event reached a relay is never published.
- A lease with no setup step for 48 hours (the longest a hold invoice lives) is canceled by the instance and
  both parties are told. This is bookkeeping: by then the buyer's payment has timed out and been refunded.
  If the seller settled but never reported the lease active, the channel and the payment stand between the
  two nodes regardless of what the instance records.
- An instance in maintenance mode accepts no new offers and no new takes; leases under way continue.

## The contract

The buyer writes the contract; both sides refer to it by its id. Canonical text (ASCII, no spaces):

```
bipnip-lease:v1:<network>:<seller node>:<buyer node>:<capacity_sat>:<duration_blocks>:<fee_base_sat>:
<fee_ppm>:<upfront_percent>:<min_confirmations>:<announce 0|1>:<payment_hash>:<created_at>
```

The lease id is the SHA-256 of that text, in hex. The fee is `fee_base_sat + floor(capacity_sat ×
fee_ppm / 1 000 000)`; the upfront share is `ceil(fee × upfront_percent / 100)` and the completion share
is the rest. Seller and instance refuse a contract whose terms are not exactly the offer's at a capacity
inside the offer's range, so nothing a buyer writes can change the price.

## Messages

Mostro protocol v2 messages with these actions; all but the first carry the order id. The payload is
`{"lease": {"<name>": {…}}}`. Unknown fields are refused.

| action | direction | payload |
|---|---|---|
| `lease-offer` | seller → instance; echoed back with the order id | `offer`: network, seller node, the seller's binding event, capacity range, duration, fee, split, confirmations, announce, expiry |
| `take-lease` | buyer → instance → seller | `take`: the contract text, the buyer's binding event, optionally the buyer's node address |
| `lease-invoice` | seller → instance → buyer | `invoice`: lease id, BOLT 11 hold invoice |
| `lease-channel-opened` | seller → instance → buyer | `channel`: lease id, funding txid, output index |
| `lease-preimage` | buyer → seller, *peer* only | `preimage`: lease id, preimage |
| `lease-active` | seller → instance → buyer | none |
| `lease-completion-invoice` | seller → instance → buyer | `invoice`: lease id, BOLT 11 invoice |
| `lease-bond-invoice` | instance → buyer, when the offer asks for a bond | `invoice`: lease id, a hold invoice on the instance's node |
| `lease-bond-claim` | instance → seller: none ("the buyer's bond is owed to you"); seller → instance: `invoice`: lease id, the seller's own invoice | |

A binding carried in a message must verify and bind that node, on that network, to the Nostr key that
sent the message. That shows the node once vouched for the key; a receiver that can reach relays still
looks for a newer revocation.

## Flow

All messages are Mostro protocol v2 messages (kind 14, NIP-44) unless marked *peer*, which go directly
between the two trade keys over the same transport.

1. **Take.** The buyer's agent generates a random 32-byte preimage `P`, computes `H = sha256(P)`, writes
   the contract (capacity, the offer's terms, both nodes, `H`) and sends `take-lease` with the order id,
   the contract, its binding event and, if needed, its node address. The instance checks the binding,
   the contract against the offer, and the rules against its own graph. If the offer asks for a bond,
   the take waits here: the instance sends the buyer a `lease-bond-invoice`, and goes on only once that
   payment is locked (an hour at most; otherwise the lease is canceled and the seller was never asked).
2. **Invoice.** The instance forwards the take to the seller. The seller's agent has its **own node**
   create a hold invoice on `H` for the upfront share, valid ≤ 48 h, `min_final_cltv_expiry_delta` within
   the instance's `payment_cltv_limit` and at least `confs` + 36 blocks (room to open, confirm and
   settle before the payment would go back), and sends it (`lease-invoice`).
3. **Pay.** The buyer's agent checks: payee = the seller's bound node key, payment hash = `H`, amount =
   the upfront share for the chosen capacity, expiry and CLTV within limits. It pays; the HTLC is held at
   the seller's node.
4. **Open.** When its node reports the invoice accepted, the seller opens the channel to the buyer
   (from its wallet or as a vortex-ng coinjoin output) and sends `lease-channel-opened` with the funding
   outpoint.
5. **Verify and release** (*peer*). The buyer's agent waits until its own node shows a channel with:
   remote = the seller's node, the agreed capacity, seller as initiator, the agreed channel type and
   announce flag, at least `confs` confirmations. Then it sends `P` to the seller's trade key
   (`lease-preimage`). Before this point `P` is never sent anywhere, and it never goes to the instance.
6. **Settle.** The seller's agent checks that `P` hashes to `H`, settles the hold invoice with it and
   reports `lease-active`.
7. **Completion.** At `funding height + duration`, the seller sends an ordinary invoice for the remaining
   share (`lease-completion-invoice`); the buyer's agent pays it if its node still sees the channel open.
8. **Outcomes.** Each party may publish a kind 8735 outcome; both may rate through Mostro's rating flow.

## The buyer's bond

An offer may ask its buyer for a bond, so that a buyer who walks away from an opened channel pays for
it. It is a hold invoice on the instance's own node, for a preimage only the instance knows, on the same
terms as a ring's bond (`liquidity-rings.md`): the buyer's agent pays it only if it pays the node the
instance's own binding names, for exactly the order's bond, with a final CLTV delta of at most 720 blocks,
and only up to the limit the buyer set on its own agent.

- **Returned** when the seller reports the lease active; when the lease is canceled before the seller
  reported a channel; and when the seller itself cancels.
- **Decided at the end of the bond window** (48 hours from the take unless the instance sets less) when
  the lease never became active after the seller reported a channel, whether the buyer canceled or just
  went quiet. The instance looks in its own node's graph for the outpoint the seller reported: between
  the two nodes, of the contract's capacity, and not there when the bond was locked. If it is there, the
  bond is taken and owed to the seller; if not, it is returned. The seller's word alone never takes it.
- **Paid on.** The instance tells the seller (`lease-bond-claim` without a payload); the seller answers
  with an invoice for the bond less the 1% routing reserve. When its own node shows that paid, the seller
  cancels the lease's hold invoice, so the buyer's upfront payment goes back: a seller is paid the bond or
  the fee, never both. An unclaimed bond stays with the instance after seven days.

What the buyer trusts the instance with is what a ring member does: the bond itself, the instance's view
of the graph, and liveness. What the seller trusts it with is being paid on a bond that was taken.

## Failure handling

| case | result |
|---|---|
| seller never opens | the seller cancels, or the HTLC times out; the buyer is refunded; negative outcome |
| channel differs from the terms | the buyer does not release `P`; the invoice expires; dispute if the seller claims otherwise |
| buyer withholds `P` after a verified open | with a bond: once the bond window has ended, an announced channel visible in the instance's own graph → the buyer's bond is paid on to the seller, who then gives the upfront payment back. Without a bond, or unannounced: the upfront payment times out and the seller keeps a channel it was not paid for; a solver may later decide from the signed messages |
| seller closes early | completion share unpaid; negative outcome |
| seller raises fees above `fee_caps` | negative outcome; buyer may close |

## Limits

- Hold windows ≤ 48 h: held HTLCs tie up liquidity on every node in the payment path.
- A seller's node must support hold invoices on an external hash (LND `AddHoldInvoice`, Core Lightning with
  a hold-invoice plugin, LDK `receive_for_hash`).
