Liquidity Rings over Mostro instances
=====================================

`draft` · bipnip, 2026-11-16 · requires: the node-binding draft, Mostro protocol v2

N node operators (2 ≤ N ≤ 5) each open one channel of the same capacity to the next member and receive one
from the previous member; the last opens to the first. No money changes hands.

This version has no bonds: a member that never opens costs the others nothing but the channel they opened,
which stands. Bonds are described at the end as the planned next step.

## Roles

- **Creator**: proposes the ring and takes its first slot.
- **Member**: takes a slot, commits to the roster, opens one channel, confirms the one opened to it.
- **Instance**: publishes the order, fills the slots, fixes the order of the members, relays, and reports how
  the ring ended. It holds nothing.

## Ring order (kind 38735, `k` = `ring`)

Published and signed by the instance after a creator's `ring-offer`, `d` = order id. Tags:

| tag | meaning |
|---|---|
| `k` | `ring` |
| `s` | `pending` (slots open), `in-progress`, `success`, `partial`, `canceled`, `expired` |
| `network` | as in the binding |
| `slots` | `<total>` `<taken>` |
| `capacity` | channel capacity in sat, the same for every channel |
| `announce` | `true` or `false` |
| `window` | seconds members have to open once the roster is fixed (LN+ uses 48 h) |
| `expiration` | NIP-40, for filling the slots |

Not in this version: `rules` (member requirements checked against the instance's graph) and `bond`.

## The roster

Once every slot is taken the instance fixes the order of the members and the time. The roster's canonical
text (ASCII, no spaces; members separated by commas, in opening order) is:

```
bipnip-ring:v1:<network>:<capacity_sat>:<announce 0|1>:<open_window_secs>:<created_at>:
<node 1>,<node 2>,…,<node N>
```

The ring id is the SHA-256 of that text, in hex. Member *i* opens to member *i+1*, and the last member to
the first. The open window ends at `created_at + open_window_secs`. Capacity is 20 000 to 1 000 000 000 sat;
the window is one hour to two weeks; no node is in a ring twice.

## Messages

Mostro protocol v2 messages with these actions; all but the first carry the order id. The payload is
`{"ring": {"<name>": {…}}}`. Unknown fields are refused.

| action | direction | payload |
|---|---|---|
| `ring-offer` | creator → instance; echoed back with the order id | `offer`: network, capacity, slots, announce, window, expiry, and the creator's own seat |
| `take-ring-slot` | member → instance | `take`: a seat (node key, the node's binding event, node address) |
| `ring-roster` | instance → every member | `roster`: the canonical text and every member's seat, in order |
| `ring-commit` | member → instance | `commit`: ring id, signature |
| `ring-open` | instance → every member | `open`: ring id and every member's commit |
| `ring-edge-opened` | opener → instance → the member it opened to | `edge`: ring id, funding txid, output index |
| `ring-edge-verified` | receiver → instance → the opener | `edge`: the same |
| `ring-closed` | instance → every member | `closed`: ring id and the positions of the members whose channel was confirmed |

A `cancel` from the creator withdraws a ring whose slots are not full; from a member it gives its slot back.

## Flow

1. **Offer.** The creator sends `ring-offer`. The instance checks the terms and that the creator's node binds
   the key that sent the offer, stores it and publishes the order.
2. **Slots.** Members send `take-ring-slot`. The instance checks each binding the same way, and that no node
   and no Nostr key takes two slots.
3. **Roster.** With the last slot taken, the instance fixes a random order and sends `ring-roster`. Each
   member's agent checks it without trusting the instance: the text parses and is the offer it saw (terms,
   size, the creator is in it), its own node is in it, the seats are the members' in order, and **every
   member's binding verifies**, which gives the Nostr key each node vouches for.
4. **Commit.** Each member signs `bipnip-ring-commit:v1:<ring id>` (Schnorr, over its SHA-256) with its
   liquidity identity and sends `ring-commit`.
5. **Open.** When the instance has every commit it sends them all in `ring-open`. A member opens **only
   after** checking that there is exactly one valid commit per member, by the key that member's node vouches
   for. It then opens a channel of the ring's capacity to the next member and reports the funding outpoint
   (`ring-edge-opened`).
6. **Verify.** The receiving member's agent looks on its own node for that outpoint: the remote node is the
   previous member, the capacity and the announce flag are the ring's, the other side opened it, it is
   confirmed, and it did not exist before the roster. It then sends `ring-edge-verified`.
7. **Close out.** When every channel is confirmed, or the window ends, the instance sends `ring-closed`:
   status `success` when all are confirmed, `partial` otherwise. The channels that were opened stand.

## What an instance cannot do

- Seat a node whose operator did not ask: a seat needs that node's signed binding to the key that asked.
- Show members different rings: everyone signs the same ring id, and nobody opens before holding every
  member's signature of it.
- Change the capacity, the announce flag, the window or the size between offer and roster: members compare
  the roster with the offer.
- Make a member believe a channel exists: the member's own node is the judge.

It can pick the order of the members, refuse to fill a ring, and withhold messages; a ring then does not
happen, or ends `partial`.

## What a member can do to the others

Commit and never open. The member it should have opened to has opened its own channel and received none.
This version only records it (`ring-closed`, and later outcomes and ratings).

## Planned: bonds

Each member pays a hold invoice to the instance before the window starts; all are canceled when the ring
closes `success`; at window end each missing channel's opener loses its bond to the member it failed. This
puts the instance in a payment path and is not part of this version.

## Atomic rings

A ring whose funding outputs are all in one transaction leaves no member able to defect. That needs every
member's node to accept an externally funded channel and a joint transaction; bipnip delegates it to
vortex-ng (`docs/executor-api.md`, `RingIntent`), once vortex-ng supports it.
