# 8. Reputation: facts a verifier checks itself

Date: 2026-11-10

## Status

Accepted

## Context

Mostro keeps 1–5 ratings per identity key per instance (kind 38384), with opt-in portability through
signed attestations. Magma and LN+ keep ratings in their own databases. Liquidity has facts anyone with a
node can check: whether a channel exists, its capacity, its age, whether it closed early.

## Decision

- Fiat reputation stays Mostro's, unchanged (per instance, kind 38384, its portability scheme).
- Liquidity reputation is computed by each verifier from: its own node's graph (channels exist, capacity,
  age, early closes), signed outcome events (kind 8735) from bound identities, and the follows of the
  people the verifier trusts. Instances also publish their ratings as Mostro does.
- Computed scores may be published as NIP-85 trusted assertions by anyone; none is authoritative.
- Optional later: a JoinMarket fidelity bond as weight against Sybil identities.

## Consequences

- Ratings are portable between instances by construction: outcomes are signed by the parties.
- Outcomes for unannounced channels are only checkable by the two parties; they weigh less.

### Notes (2026-11-18): the first outcomes and the first record

- The outcome event is specified in `docs/nips/liquidity-outcomes.md`: regular (an author cannot replace
  what it said), content = the author's node binding, dated when the author's run ended. Agents write
  one when a lease ends (kept, closed early, unpaid) and for the ring edge opened to them; instances
  write ring edges and taken lease bonds, never a lease's early close.
- The agent's record (`bipnip record NODE`) counts by the draft's reading rules: a kept lease needs both
  parties; an early close counts against a node only by its own word or both parties'; at most one
  positive per pair of nodes per 2016 blocks; disputes and distinct counterparties are shown; there is
  no score and no ratio. The verifier's own graph does not confirm outcomes yet, and NIP-85 assertions
  and Mostro's per-instance ratings (38384, bound to the `orders` table) are not wired to liquidity.
- Rules on takers (`min_channels`, `min_capacity_sat`) are checked by the instance in its graph and
  again by the counterparty's agent in its own; a buyer's agent can hold sellers to its own minimums
  (`[policy] min_seller_*`).
