Liquidity Rings over Mostro instances
=====================================

`draft` · bipnip, 2026-11-10 · requires: the node-binding draft, Mostro protocol v2

N node operators (2 ≤ N ≤ 5) each open one channel of the same capacity to the next member and receive one
from the previous member. No money changes hands; optional bonds give defection a cost.

## Ring order (kind 38735, `k` = `ring`)

Published and signed by the instance after a creator's `NewOrder`, `d` = order id. Tags:

| tag | meaning |
|---|---|
| `k` | `ring` |
| `s` | `pending` (slots open), `in-progress`, `success`, `partial`, `canceled`, `expired` |
| `network` | as in the binding |
| `slots` | `<total>` `<taken>` |
| `capacity` | channel capacity in sat, the same for every edge |
| `announce` | `true` or `false` |
| `window` | seconds members have to open once the roster is fixed (LN+ uses 48 h) |
| `rules` | member requirements, `key=value` (`min_channels`, `min_capacity`, `min_age_days`, `clearnet`, `tor`, `min_score`) |
| `bond` | bond per member in sat, or `0` |
| `expiration` | NIP-40, for filling the slots |

## Flow

1. **Take a slot** (`TakeRingSlot`): binding event and node address. The instance checks the binding and
   the rules against its own graph.
2. **Roster.** When all slots are taken, the instance fixes a random order and sends the roster (members'
   node keys and addresses, in order) to every member (`RingRoster`). Each member signs the roster with
   its liquidity identity (`RingCommit`) and, if `bond > 0`, pays its bond hold invoice to the instance.
3. **Open.** Once every commit (and bond) is in, the window starts. Member *i* opens a channel of
   `capacity` to member *i+1* (the last to the first) and sends `RingEdgeOpened` with the funding
   outpoint.
4. **Verify.** Member *i+1*'s agent checks the incoming channel on its own node (remote = member *i*,
   capacity, initiator, announce flag, at least 1 confirmation) and sends `RingEdgeVerified`.
5. **Close out.** All edges verified → bonds cancelled (refunded), status `success`. At window end with
   edges missing → each missing edge's opener loses its bond to the member it failed, status `partial`;
   the opened channels stand. Outcomes (kind 8735) and ratings follow.

## Atomic rings

A ring whose funding outputs are all in one transaction leaves no member able to defect. That needs every
member's node to accept an externally funded channel and a joint transaction; bipnip delegates it to
vortex-ng (`docs/executor-api.md`, `RingIntent`), once vortex-ng supports it.
