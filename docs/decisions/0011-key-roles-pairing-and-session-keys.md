# 11. Key roles, pairing and session keys

Date: 2026-11-18

## Status

Accepted. Amends ADR 0009 point 2 (pairing) and ADR 0010 (the agent's local API gains a second
socket). Built for the web client's Liquidity tab (W1).

## Context

ADR 0009 makes the agent the security boundary for the web client: the page is untrusted and
holds no key of the agent's; it gets a session key through a pairing ceremony, with scoped
permissions, revocable at the agent. ADR 0010 built the daemon with one unix socket and no pairing.

Three facts shaped how pairing is built:

- **The page has no secure context.** The web host is opened on a tailnet address over plain HTTP
  (`http://100.64.0.3:<port>`). Browsers treat that as insecure: no `crypto.subtle`, so no
  non-extractable WebCrypto key, and no `Sec-Fetch-*` headers. Whatever key the page holds, page
  script can read it.
- **The spending policy does not weigh everything a run can cost.** `[policy]` limits fees,
  capacity, bonds per run and runs at once. It does not weigh the funding transaction's fee rate,
  the price a lease is sold at, or bonds per day. A request within the limits is not thereby
  harmless when a hostile page made it.
- **The daemon's engine is one task.** A request that waits on a relay holds everything else up.

## Decision

### Key roles

| key | from | used for | leaves the agent |
|---|---|---|---|
| mnemonic | `<data_dir>/mnemonic`, owner-only | everything below | never |
| fiat identity, trade keys | NIP-06 `m/44'/1237'/38383'/0/{0,N}` | Mostro fiat trades (kept unlinkable) | public keys only |
| liquidity identity | `…/38735'/0/0` | binding (31735), outcomes (8735), the record | public key only |
| liquidity trade keys | `…/38735'/0/N` | one per lease or ring | public keys only |
| node key | the Lightning node | signs the binding and its revocation | never (the node signs) |
| session key | made by a web page | signs that page's requests | the page holds it |

What is absent: a hardware or remote signer for the identities, and rotation (moving a record to
a new key). Proving trust without naming the key is research (issue #47).

### The web door

A `[web]` section opens a second socket, the web door (`<data_dir>/web.sock`, or a path of its
own). Without it there is none. Placed outside the data directory, in a directory that belongs to
the agent's user (or root) and that nobody else may write to, with `socket_mode = 0o660` and the
web host's user in `allow_uids`, it lets a web host run as another user, who can reach this socket
and none of the agent's files. The door has its own connections (8) and its own queue; the daemon
takes the operator's requests first, then the runs' tick, then the door's.

### Signed requests

Every line on the web door is a Nostr event of kind 24735 (ephemeral range; never published),
signed with the page's session key. Its content is an ordinary request line; a `p` tag names the
agent's liquidity identity; a `nonce` tag keeps two equal requests apart. The connection checks
the event's id, signature and kind. The engine then checks, in order:

1. fresh: dated within 120 s before and 5 s after the agent's clock, and not before the agent's
   start (plus 5 s), so nothing seen before a restart can be replayed after it; a `stale` answer
   carries the agent's time, and the page corrects its clock offset;
2. not seen before (each key's recent event ids are kept for the freshness window);
3. within the key's rate (60 a minute);
4. addressed to this agent (only `pair.request` and `pair.status` may name no one);
5. from a key with a live session (else `unpaired`), whose scope covers the request.

The web host only passes the lines on. It holds no key, and verification stays in the process that
is the boundary.

### Pairing

Only while the operator holds a window open (`bipnip pair open`, 3 minutes by default, 10 at
most) may a page ask (`pair.request`). At most 4 wait; none is pushed out by a newer one. The page
is given a twelve-digit code and shows it; the operator types it into `bipnip pair approve CODE
[--scope read|trade] [--expires DAYS] [--label NAME]`. Typing the code is the comparison: the
agent never lists codes, so an operator cannot pick another page's from a list. A session is
`read` unless the operator says `trade`, and lasts 7 days unless the operator says otherwise (90
at most). `bipnip sessions` lists them, `bipnip revoke SESSION` ends one, and a page can end its
own (`pair.end`).

### Scopes

- `read`: `status`, `orders` (with sellers' records), `lease.list`, `ring.list`, `approvals` (its
  own), `watch` (two at once), `pair.status`, `pair.end`.
- `trade`: also `lease.offer`, `lease.take`, `ring.create`, `ring.join`, and cancelling runs that
  session asked for.
- Never from a page: `approve`, `deny`, pairing, `sessions`, `revoke`, `binding.publish`, and
  `record` of a node the page names (it would go to every relay in a filter).

The scope table is one exhaustive `match`: a new request does not compile until it is placed.

### A page moves no money by itself

Every run a page asks for is parked, whatever the limits: the operator approves it with `bipnip
approve`, which shows what the request would do as the agent stored it (method, parameters, what
it would pay, open and lock, with which node, why it was parked, which page asked) and asks y/N.
The page's own display of a price is a claim; the prompt is what is true. A page may have 3
requests waiting, asks for a run at most every 10 s, and its waiting requests lapse after 24 h or
when its session ends. An approved run's grant names the page and the approval.

### What a page is shown

`status` without the policy or the day's use; lists without store file names; runs without their
one-line summary and without the outpoint of a channel that is not announced.

## Consequences

- A page, or whatever takes it over (cross-site script, a hostile dependency in the page's
  bundle, someone else on the tailnet holding a copied session key), can read what its scope
  shows and park requests. It cannot move money, approve, pair or revoke.
- The session key can be read by script in the page. A copy is useful only to someone who can
  reach the web host, and then only as far as above, until the session expires or is revoked.
  TLS on the tailnet would give the page a secure context and a key it cannot copy (later).
- One terminal command per trade: the price of the operator seeing what the agent will really do.
  Trades from a page within limits come once the policy weighs fee rate, sale price and bonds per
  day.
- Relay reads (`orders`, `record`) run beside the engine, so neither a page nor a slow relay holds
  up the operator or the runs.
