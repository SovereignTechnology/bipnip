# 5. Event kinds and transport for liquidity

Date: 2026-11-10

## Status

Accepted; kind numbers provisional until the NIP drafts are agreed

## Context

Mostro uses kinds 38383 (NIP-69 orders), 38384 (ratings), 38385 (instance info), 38386 (disputes), 38387,
38389, 8383 and, for messages, kind 14 events with NIP-44 content signed by per-trade keys (transport v2;
NIP-59 gift wraps were removed in v0.19.0). Other liquidity-over-Nostr work: publsp (kind 39735 offers,
signed by the LSP itself, LSP-held preimage) and the ohstr NIP-LSP draft (kind 10735). The NIPs registry
(master `b587c619`, 2026-11-10) lists none of the numbers chosen below.

Upstream Mostro apps must never show a liquidity order as a fiat order.

## Decision

| kind | what | signed by |
|---|---|---|
| 31735 | node binding (addressable, `d` = node key) | the bound Nostr key |
| 38735 | liquidity order (addressable, `d` = order id; tag `k` = `lease` or `ring`) | the instance, as Mostro signs 38383 |
| 8735 | liquidity outcome (regular): a party's signed account of a finished lease or ring edge | the party's liquidity identity |

- Liquidity messages between agent and instance use Mostro's transport v2 unchanged (kind 14, NIP-44,
  trade key + optional identity proof); the identity proven for liquidity trades is the node-bound key
  (ADR 0004). New `Action`/`Payload` variants carry the lease and ring steps.
- Messages between counterparties (the preimage release, ring edge reports to the successor) go
  directly between trade keys over the same transport, as Mostro's peer messages do.
- **Not publsp's 39735:** publsp offers are signed by the seller and assume the seller holds the
  preimage; bipnip orders are published by an instance and the buyer holds the preimage (ADR 0003).
  A read-only bridge and a unification proposal come later (M6).
- Numbers near 9735 (the Lightning port) are mnemonic only; any can change before the drafts are posted.

## Consequences

- Upstream apps ignore kinds they do not know, so wire compatibility for fiat is untouched.
- Relays see which trade key talks to which instance (as in Mostro v2), not the content.
