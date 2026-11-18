Liquidity Rings over Mostro instances
=====================================

`draft` · bipnip, 2026-11-16 · requires: the node-binding draft, Mostro protocol v2

N node operators (2 ≤ N ≤ 5) each open one channel of the same capacity to the next member and receive one
from the previous member; the last opens to the first. No money changes hands between members.

A ring may ask every member for a **bond**: a payment locked at the instance's node before anyone opens,
returned when the member's channel is there, and paid on to the member it let down when it is not (see
[Bonds](#bonds)). Without a bond, a member that never opens costs the others nothing but the channel they
opened, which stands.

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
| `bond` | what every member locks at the instance, in sat; `0` for none |
| `expiration` | NIP-40, for filling the slots |

| `rules` | optional: `min_channels=<n>`, `min_capacity_sat=<sat>`, one value per rule set, in that order. What every member's node must show in the public graph; absent when none is set |

A ring with rules writes them into its roster text (`bipnip-ring:v2:…`), so every member's commit covers
them. The instance checks every member's node against the rules in its own graph when a slot is taken, and
each member's agent checks the roster against its own node's graph before it commits.

## The roster

Once every slot is taken the instance fixes the order of the members and the time. The roster's canonical
text (ASCII, no spaces; members separated by commas, in opening order) is:

```
bipnip-ring:v1:<network>:<capacity_sat>:<announce 0|1>:<open_window_secs>:<bond_sat>:<created_at>:
<node 1>,<node 2>,…,<node N>
```

The ring id is the SHA-256 of that text, in hex. Member *i* opens to member *i+1*, and the last member to
the first. The open window ends at `created_at + open_window_secs`. Capacity is 20 000 to 1 000 000 000 sat;
the window is one hour to two weeks (on regtest, one minute); no node is in a ring twice. A bond is 0 or
at least 1 000 sat and at most the capacity, and a ring with a bond has announced channels and a window of
at most 48 hours.

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
| `ring-closed` | instance → every member | `closed`: ring id, the positions of the members whose channel was confirmed, and the positions of those whose bond was taken |
| `ring-bond-invoice` | instance → each member, after the commits, in a ring with a bond | `invoice`: ring id and a hold invoice on the instance's node |
| `ring-bond-claim` | a member that is owed a bond → instance | `invoice`: ring id and the member's own invoice |

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
5. **Bonds** (only in a ring with a bond). When the instance has every commit it sends each member a
   `ring-bond-invoice`. The member's agent pays it only if it pays **the node the instance's own binding
   names**, for exactly the bond in the roster, on the ring's network, with a final CLTV delta of at most
   720 blocks. A bond not paid within an hour cancels the ring; the bonds already locked are returned.
6. **Open.** When the instance has every commit (and, in a ring with a bond, every bond is locked) it sends
   the commits in `ring-open`. A member opens **only
   after** checking that there is exactly one valid commit per member, by the key that member's node vouches
   for. It then opens a channel of the ring's capacity to the next member and reports the funding outpoint
   (`ring-edge-opened`).
7. **Verify.** The receiving member's agent looks on its own node for that outpoint: the remote node is the
   previous member, the capacity and the announce flag are the ring's, the other side opened it, it is
   confirmed, and it did not exist before the roster. It then sends `ring-edge-verified`.
8. **Close out.** When every channel is confirmed, or the window ends, the instance sends `ring-closed`:
   status `success` when all are confirmed, `partial` otherwise. The channels that were opened stand.
   Bonds are returned or taken as described below.

## What an instance cannot do

- Seat a node whose operator did not ask: a seat needs that node's signed binding to the key that asked.
- Show members different rings: everyone signs the same ring id, and nobody opens before holding every
  member's signature of it.
- Change the capacity, the announce flag, the window or the size between offer and roster: members compare
  the roster with the offer.
- Make a member believe a channel exists: the member's own node is the judge.

It can pick the order of the members, refuse to fill a ring, and withhold messages; a ring then does not
happen, or ends `partial`. In a ring with a bond it also holds every member's bond until the ring ends, and
it is the judge of whose bond is taken: see the limits of that below.

## What a member can do to the others

Commit and never open. The member it should have opened to has opened its own channel and received none.
Without a bond this is only recorded (`ring-closed`, and later outcomes and ratings). With a bond, the
member that never opened pays for it.

## Bonds

A bond is a hold invoice on the instance's own node, for a preimage only the instance knows. The instance
publishes a node binding for its own key (the node-binding draft), so members know which node that is.

- **Locked before anyone opens.** No member is told to open until every member's bond is locked.
- **Returned** (the invoice is cancelled) when every channel was confirmed; and, at the end of the window,
  to each member whose channel was confirmed by the member it opened to, **or is visible to the instance's
  own node in the public graph**: a channel between the two nodes, of the ring's capacity, that was not
  there when the bonds were locked, that no other member reported as its own, and, if the member reported
  an outpoint, at that outpoint.
- **Taken** (the invoice is settled) otherwise, and owed to the member that should have received the
  channel. `ring-closed` lists the positions whose bond was taken. The member owed sends `ring-bond-claim`
  with an invoice for the bond less a routing reserve of 1% (rounded up); the instance pays it with the fee
  capped at that reserve and keeps what is left of the reserve. A bond not claimed within seven days stays
  with the instance.

Why the graph: so that one member's word can never take another member's bond. A member that opened is safe
from a neighbour that denies the channel, because the instance looks for itself; and a member that did not
open cannot be saved by a neighbour's silence. This is also why a ring with a bond needs announced channels,
and a window short enough to hold a payment for (48 hours).

What a member trusts the instance with when it joins a ring with a bond, and no check can remove:

- **The bond itself.** The instance can settle a bond it should have returned, or keep one it should have
  paid on. The loss is bounded by the bond, and by the limit the member sets on its own agent.
- **Its view of the graph.** A channel that is open but has not reached the instance's node by the end of
  the window (six confirmations, then gossip) counts as missing unless the receiving member confirmed it.
- **Liveness.** If the instance disappears, a locked bond comes back when the payment times out (the
  invoice's CLTV delta, at most 720 blocks).

Two known gaps in this version: a member whose own report was lost, in a ring of two where the other
member did open, is not told apart from that other member and keeps its bond; and a channel opened and
closed again inside the window still counts.

## Atomic rings

A ring whose funding outputs are all in one transaction leaves no member able to defect. That needs every
member's node to accept an externally funded channel and a joint transaction; bipnip delegates it to
vortex-ng (`docs/executor-api.md`, `RingIntent`), once vortex-ng supports it.
