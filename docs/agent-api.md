# The agent's local API

How anything else on the machine talks to a running `bipnip-agent` (`bipnip-agent run`): the `bipnip`
command line today, the web client later. Decision: `docs/decisions/0010-agent-daemon-and-local-api.md`.

## Transport

A unix socket at `<data_dir>/agent.sock`, mode 0600, owned by the user that runs the agent. The data
directory must be mode 0700, or the agent does not start. No network listener.

Newline-delimited JSON (UTF-8, one object per line, at most 64 KiB a line). **One request per connection**:
the client writes one line; the agent writes zero or more *status* lines and then exactly one *final* line,
and closes.

```json
{"v": 1, "method": "lease.take", "params": {"order_id": "…", "capacity_sat": 600000}}
```

```json
{"status": "started", "run": "3f9a…"}
{"ok": true, "result": {"run": "3f9a…", "state": "taking"}}
```

A failure ends the connection with `{"ok": false, "error": {"code": "…", "message": "…"}}`. Codes:
`bad-request` (not JSON, unknown method, a missing or malformed parameter), `not-found`, `refused` (a
check failed: the message says which), `needs-approval` (parked; `error.approval` names it), `busy` (too
many connections, or a page asking too often), `node` (the Lightning node could not be asked), `relay`,
`internal`, and at the web door `unpaired` and `stale` (below).

A client has ten seconds to send its request line. At most 16 connections are served at once. A client
that reads slower than the agent writes loses lines; the agent never waits for a client.

Unknown fields are errors. Amounts are integers in satoshi unless the name ends in `_msat`.

## What never crosses the socket

The mnemonic, any Nostr secret key, the node's macaroon or certificate, and any payment preimage. A
response names runs, orders, states, amounts, public node keys and outpoints.

## Methods

| method | params | result |
|---|---|---|
| `status` | none | agent version, network, node key and sync state, instances, counts of runs by state, today's use of each limit |
| `orders` | `kind`?: `lease` \| `ring`; `instance`?; `capacity_sat`?; `max_fee_sat`?; `records`? | one book over the configured instances (see below) |
| `lease.offer` | `min_capacity_sat`, `max_capacity_sat`, `duration_blocks`, `fee_base_sat`, `fee_ppm`, `feerate_sat_vb`, `upfront_percent`?, `min_confirmations`?, `announce`?, `expires_in_secs`?, `bond_sat`?, `instance`? | `run`, `state` |
| `lease.take` | `order_id`, `capacity_sat`, `node_address`?, `instance`? | `run`, `state` |
| `lease.cancel` | `run` | `state` |
| `lease.list` | none | every stored lease run: `run`, role, `order_id`, `state`, lease id, channel, bond, what it waits on |
| `ring.create` | `capacity_sat`, `slots`, `feerate_sat_vb`, `announce`?, `open_window_secs`?, `expires_in_secs`?, `bond_sat`?, `node_address`?, `instance`? | `run`, `state` |
| `ring.join` | `order_id`, `feerate_sat_vb`, `node_address`?, `instance`? | `run`, `state` |
| `ring.cancel` | `run` | `state` |
| `ring.list` | none | every stored ring run |
| `binding.publish` | none | the binding event's id and the relays that took it |
| `approvals` | none | the parked requests: `approval`, what was asked, which limit it is over and by how much |
| `record` | `node` (66 hex) | `record`: the node's record from signed outcomes on the relays (`kept`, `opened`, `missing`, `closed_early`, `disputed`, `unpaid`, `counterparties`, `subjects`), and `outcomes_read` |
| `approve` | `approval` | the run it started |
| `deny` | `approval` | none |
| `watch` | `run`? | no result until the client disconnects; a status line for every change of state of the run, or of every run |

`run` is the name the run is stored under: `seller-…`, `buyer-…` or `ring-…` and hex. A request that
starts a run writes one status line, `{"status": "started", "run": "…"}`, and answers as soon as the run
is saved and its first message is on its way; the run then goes on in the agent. `watch` follows it: it
first writes a `{"status": "run", …}` line for each run under way, then `{"status": "watching"}`, then a
`run` line for every change and a `{"status": "problem", "run": "…", "problem": "…"}` line for whatever
went wrong in a step. It ends when the client closes the connection.

`orders` reads every configured instance's order events in one request over the agent's relay connection,
keeps the newest version of each (an order is named by its instance and its id together) and only events
the instance signed, then lists the open ones on the agent's network: leases first, cheapest first by
`ppb_per_block` (the whole fee per satoshi of capacity per block, in parts per billion, at `capacity_sat` if
given, else at the largest capacity the order offers), then rings, largest channel first. `capacity_sat` keeps
leases that offer it and rings at least that large; `max_fee_sat` keeps leases whose whole fee at that
capacity is at most it. Each lease also gives `priced_capacity_sat`, `fee_sat` and `seller_listings` (how
many open leases its seller's node has on the book). The answer names `instances_silent` (no order event
from them at all) and `relays_down` (relays the agent is not connected to) instead of failing. With
`records: true` each lease also carries `seller_record`, its seller's record counted as `record` counts it,
from one more relay request for all the book's sellers at once; if that request fails, the book is still
answered, with `records_failed` saying why.

`orders` and `record` only read the relays, so the agent answers them beside its runs rather than between
them: a relay that is slow to answer holds up no run and no other request.

A run is described the same way everywhere (`lease.list`, `ring.list`, `watch`, the answer to a request
that starts one): `run`, `role` (`seller`, `buyer` or `member`), `order_id`, `state`, `finished`,
`watching`, and `line`, the one-line summary the one-shot commands print; then, by role, `lease`, `ring`,
`channel`, `bond`, `owed_bond`, `waiting_on`, and the numbers: a seller's `offered` (the capacities,
length, fee, upfront share, bond and rules it offered), a lease's `terms` once there is a contract
(`capacity_sat`, `fee_sat`, `upfront_sat`, `duration_blocks`, `announce`, `peer`: the other party's node,
and `ends_at_height` once the channel is confirmed), a ring member's `terms` (`capacity_sat`, `bond_sat`,
`announce`, `open_window_secs`, and `members` once the roster is fixed). The lists add `under_way` and, for a run the agent had to
stop because it could not be saved, `halted`.

A lease that is active is `watching` until it ends: the agent looks at the leased channel on its own node
once a minute until the lease has run its length (then bills or pays the completion share) or the channel
is gone. Such a run is neither `finished` nor `under_way`: it holds no slot under `max_runs`. `status`
counts them under `runs.watching`. A lease ends `complete` (it ran its length; the completion share, if
any, is paid), `closed` (the channel went first, or the node cannot tell when), `unpaid` (a seller billed
and was never paid) or `unbilled` (a buyer was never billed).

A run that finished more than 30 days ago is put away: its files move into a `done` directory beside the
others (`<data_dir>/leases/done`, `rings/done`, `policy/done`). Nothing is deleted, and such a run is no
longer listed.

When a lease ends (kept, closed early, unpaid) or a ring the agent sat in ends, the agent writes a signed
outcome (kind 8735, `docs/nips/liquidity-outcomes.md`) with its liquidity identity: for announced channels
by default (`[policy] publish_outcomes`). A ring member writes up the edge opened to it. `record` counts a
node's outcomes by the draft's reading rules, trusting the configured instances; it does not yet confirm
an outcome from this agent's own graph.

## Policy

```toml
[policy]
max_fee_per_lease_sat = 0        # a buyer's whole fee for one lease, upfront and completion
max_fee_per_day_sat = 0          # all leases taken in a day
max_capacity_per_day_sat = 0     # channels this agent opens in a day: leases sold, rings
max_bond_sat = 0                 # the largest bond locked at an instance
max_runs = 0                     # runs under way at once (not finished, not only watched)
allowed_peers = []               # node keys; empty means any
min_seller_channels = 0          # a take parks when the seller's node shows fewer public channels
min_seller_capacity_sat = 0      # in our node's graph, or less public capacity (0: no minimum)
```

Every limit defaults to 0. With the defaults every request that starts a run is parked as
`needs-approval`; nothing above a limit happens without `approve`. A parked request has stored, sent
and published nothing: an offer's node binding, which ties the node to the agent's identity in public,
goes out only once the offer is allowed.

A request is weighed by what it would do with the node's money: the whole fee of a lease it takes, the
capacity of the channel it would open (for an offer, the largest capacity offered), and the bond it would
lock. That is checked twice:

1. When the run is asked for, against the limits and against what the last 24 hours already used. Every
   run the agent starts is recorded with what it was granted (`<data_dir>/policy/`), before its first
   message leaves; the day's use is the sum of those records, so it holds across restarts. A run counts
   from the moment it starts, whether or not it ends up paying or opening.
2. At the node. A run reaches the node only through its grant: the invoices it pays may add up to its fee
   and its bond and no more, the channels it opens to the capacity it was granted and no more, and with
   `allowed_peers` set it opens only to those. Routing fees are capped apart, per payment. A payment the
   node reports as failed gives its share back, so an invoice that expired and was replaced is not
   counted twice.

An approval is for one request as it was shown: if the order has changed so that the request would now
pay, open or lock more, or deal with another node, the approval is refused and the request must be asked
again. An approval does not raise a limit.

A run started by a one-shot command (`bipnip-agent lease …`, `ring …`) has no grant. The agent carries it
on as that command would have; the person who typed it gave it its limits. Of the limits above, only
`max_bond_sat` applies to the one-shot commands, which refuse a bond over it.

## Who may connect

On `agent.sock`: anyone who can open the socket, by its mode the user that runs the agent. That is the
operator, and the operator may ask for everything here.

## The web door

With a `[web]` section the agent opens a second socket for web pages (ADR 0011):

```toml
[web]
socket = "/run/bipnip-web/web.sock"  # default <data_dir>/web.sock
socket_mode = 0o660                  # default 0o600
allow_uids = [1001]                  # the web host's user, besides the agent's own
```

Its directory must belong to the agent's user (or root) and be writable by nobody else, or the agent does
not start. In the data directory it works, with a warning: only a web host of the agent's own user can
reach it there, and that user can read the agent's files.

The same framing, but each line is a Nostr event of kind 24735, signed with the page's session key and
never published: `content` is a request line as above, a `p` tag names the agent's liquidity identity, a
`nonce` tag keeps equal requests apart. A request must be dated within 120 s before and 5 s after the
agent's clock (and after its start), must not have been seen before, and must come from a key with a live
session whose scope covers it. New codes: `unpaired` (no live session: pair again) and `stale` (the
answer's `error.now` is the agent's time).

| method | who | params | result |
|---|---|---|---|
| `pair.open` | operator | `secs`? (default 180, at most 600) | `open_until` |
| `pair.list` | operator | none | `open`, `open_until`, `waiting`: labels and times (never codes) |
| `pair.approve` | operator | `code`, `scope`?: `read` \| `trade`, `expires_days`? (default 7), `label`? | the session |
| `pair.deny` | operator | `code` | none |
| `sessions` | operator | none | every session: key, label, scope, times, `live` |
| `revoke` | operator | `session` (the key, 64 hex) | `ended`, `dropped` (its parked requests) |
| `pair.request` | page | `label`? | `code` to show, `agent` (whom to name in `p`) |
| `pair.status` | page | none | `paired` (with scope, label, expiry), or `pending` with its `code` |
| `pair.end` | page | none | ends its own session |

A `read` session may ask `status`, `orders`, `lease.list`, `ring.list`, `approvals` (its own) and `watch`
(two at once); a `trade` session also `lease.offer`, `lease.take`, `ring.create`, `ring.join` and the
cancel of runs it asked for. Never: `approve`, `deny`, pairing, `sessions`, `revoke`, `binding.publish`,
`record`. Every run a page asks for is parked, whatever the policy; `bipnip approve` shows what it would do
and asks. A page may have 3 waiting, asks for a run at most every 10 s, and its waiting requests lapse
after a day or when its session ends. A page is shown `status` without the policy and the day's use, lists
without file names, and runs without `line` and without the outpoint of an unannounced channel; `mine`
marks the runs it asked for. A run approved from a parked request names it in `approval`.
