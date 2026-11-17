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
many connections), `node` (the Lightning node could not be asked), `relay`, `internal`.

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
| `orders` | `kind`?: `lease` \| `ring`; `instance`? | the open orders of the configured instances, each read from the relays and verified as the one-shot commands do |
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
| `approve` | `approval` | the run it started |
| `deny` | `approval` | none |
| `watch` | `run`? | no result until the client disconnects; a status line for every change of state of the run, or of every run |

`run` is the name the run is stored under: `seller-…`, `buyer-…` or `ring-…` and hex. A request that
starts a run writes one status line, `{"status": "started", "run": "…"}`, and answers as soon as the run
is saved and its first message is on its way; the run then goes on in the agent. `watch` follows it: it
first writes a `{"status": "run", …}` line for each run under way, then `{"status": "watching"}`, then a
`run` line for every change and a `{"status": "problem", "run": "…", "problem": "…"}` line for whatever
went wrong in a step. It ends when the client closes the connection.

A run is described the same way everywhere (`lease.list`, `ring.list`, `watch`, the answer to a request
that starts one): `run`, `role` (`seller`, `buyer` or `member`), `order_id`, `state`, `finished`, and
`line`, the one-line summary the one-shot commands print; then, by role, `lease`, `ring`, `channel`,
`bond`, `owed_bond`, `waiting_on`. The lists add `under_way` and, for a run the agent had to stop because
it could not be saved, `halted`.

## Policy

```toml
[policy]
max_fee_per_lease_sat = 0        # a buyer's whole fee for one lease, upfront and completion
max_fee_per_day_sat = 0          # all leases taken in a day
max_capacity_per_day_sat = 0     # channels this agent opens in a day: leases sold, rings
max_bond_sat = 0                 # the largest bond locked at an instance
max_runs = 0                     # runs that are not finished, at once
allowed_peers = []               # node keys; empty means any
```

Every limit defaults to 0. With the defaults every request that starts a run is parked as
`needs-approval`; nothing above a limit happens without `approve`.

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

Anyone who can open the socket: by its mode, the user that runs the agent. There is no pairing and no
per-client scope yet; those come with the web client (ADR 0009), and this API will then sit behind them.
