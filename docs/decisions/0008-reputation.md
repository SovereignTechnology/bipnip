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
