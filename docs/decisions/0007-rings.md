# 7. Liquidity rings

Date: 2026-11-10

## Status

Accepted (normative text: `docs/nips/liquidity-rings.md`)

## Context

On LightningNetwork.plus, 2–5 verified node owners each open one channel to the next member and receive
one from the previous; the creator sets rules (minimum channels and capacity, clearnet or Tor, rank);
members have 48 hours to open; defection costs ratings and bans, nothing else. JoinMarket-NG's JMP-0014
rings are the same topology executed as one coinjoin (private taproot channels, for privacy).

## Decision

- The instance coordinates: ring order (kind 38735, `k=ring`: slots 2–5, channel amount, rules,
  announce flag, open window), slot takes (binding and rules checked against the instance's own graph),
  a roster in a random order fixed by the instance and counter-signed by every member, then the open
  window.
- Each member opens to its successor and reports the funding outpoint; the successor's agent verifies the
  incoming channel on its own node and reports the edge verified.
- **Optional bonds** (Mostro's bond feature, per ring): each member's bond is held by the instance for
  the window. All edges verified → bonds cancelled. A missing edge at window end → that member's bond is
  slashed to the member it failed; the other channels stand.
- Atomic rings, where every funding output is in one transaction so nobody can defect, come from
  vortex-ng as an executor (`docs/executor-api.md`, `RingIntent`).

## Consequences

- Rings need no payment, so they work before leases are trusted, and bonds give defection a cost beyond
  ratings.
- Bonds make the instance briefly custodial for the bond amount; rings without bonds have none.
