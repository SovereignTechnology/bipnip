Liquidity Outcomes
==================

`draft` · bipnip, 2026-11-18 · requires: the node-binding draft; the lease and ring drafts

How a lease or a ring edge ended, signed by the party that saw it end, so that anyone can build a record of
a Lightning node from statements that are checkable and that its counterparties made. No score is defined
here: every reader computes its own (`docs/decisions/0008-reputation.md`).

## The event (kind 8735, regular)

Signed by the author's **liquidity identity**: the Nostr key its node is bound to. The `content` is the
author's node binding event (kind 31735) as JSON, so a reader checks the author against a node without a
second lookup. An instance writing about a ring or a bond it coordinated signs with the instance key and
leaves `content` empty; readers trust such statements only from instances they chose.

The event is regular, not addressable: an author cannot replace what it said after reading the other side.
An author writes one outcome per lease or ring edge. Its `created_at` is the time the author's run ended, so
an outcome sent again after a restart is the same event.

| tag | meaning |
|---|---|
| `k` | `lease` or `ring` |
| `x` | the lease id, or the ring id (64 hex) |
| `network` | as in the binding |
| `n` | a node and its role in the deal: `["n", <node hex>, <role>]`, two of them (see below) |
| `result` | how it ended (see below) |
| `closer` | for `closed-early` only: the role of the node that closed, as the author's node saw it, or `unknown` |
| `height` | optional: `["height", <funding block>, <close block>?]`, as the author's node saw them |
| `outpoint` | optional, announced channels only: the funding outpoint `txid:vout` |
| `instance` | optional: the instance that coordinated, hex |

A lease names its `seller` and its `buyer`; a ring edge names its `opener` (the member that owed the channel)
and its `receiver`. The author is one of the two, or an instance.

| `k` | `result` | meaning |
|---|---|---|
| `lease` | `kept` | the channel stood until the lease had run its length |
| `lease` | `closed-early` | the channel closed before the lease had run its length |
| `lease` | `unpaid` | the lease ran its length and the buyer never paid the completion share it was billed |
| `lease` | `slashed` | the buyer's bond was taken (instance only) |
| `ring` | `opened` | the opener's channel to the receiver was there by the end of the window |
| `ring` | `missing` | it was not |

Nothing else is a valid outcome. A lease that ended with the channel gone and the author unable to tell when
is not written up.

## Reading outcomes

A reader:

1. checks the event, then that `content` binds the author to one of the two nodes (or that the author is an
   instance it trusts);
2. keeps one outcome per author and subject (`x`, and for rings the two nodes); an author that wrote two
   different ones for the same subject counts for nothing on it;
3. counts an outcome as **confirmed** when both parties wrote the same result, or a trusted instance did, or
   the reader's own node shows the channel (`outpoint`) as the result says;
4. counts `closed-early` against a node only when that node's own outcome says so, or both parties name it
   as the closer; otherwise it is shown as disputed;
5. counts at most one `kept` or `opened` between the same two nodes per 2016 blocks, shows how many distinct
   counterparties a node has, and never folds negatives into a ratio.

Two nodes of one operator can write `kept` about each other; each such statement costs them a channel open,
its close and the capital locked for the lease. The rules above bound that cost-free part; they do not
remove it.

## Privacy

Outcomes link the two nodes' Nostr identities to each other and to a channel. Agents write them for announced
channels only by default; the graph already shows those channels and their ends. An unannounced channel's
outcome names no `outpoint` and is written only when the operator asks.
