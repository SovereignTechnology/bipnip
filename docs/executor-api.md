# Executor API: how bipnip asks for a channel open

bipnip agrees *who* opens *what* to *whom*; an executor makes the funding transaction. Two executors:

| executor | how | when |
|---|---|---|
| `PlainNode` | the node's own `open_channel` (`bipnip-ln`) | M1 onward, every backend |
| `VortexNg` | a local request to `vortex-ngd` | when vortex-ng's Lightning milestone (its M4) lands; atomic rings after its M5 |

vortex-ng is a separate project with its own rules (JoinMarket semantics, Nostr transport only). bipnip
talks to it across a process boundary and never shares crates with it, so either can release alone. This
file is the contract both sides build to; vortex-ng adopts it through its own ADR when it starts the work.

## Transport

A unix socket owned by the user running both daemons (mode 0600), newline-delimited JSON, one request per
connection, then a stream of status lines. No network listener.

## `OpenIntent`: one channel (a lease, or one ring edge)

```json
{
  "type": "open_intent",
  "version": 1,
  "intent_id": "<uuid v4, chosen by bipnip>",
  "peer_node": "<66 hex>",
  "peer_addr": "<host:port or onion:port>",
  "capacity_sat": 2000000,
  "announce": true,
  "channel_type": "anchors",
  "deadline_unix": 1762905600
}
```

- vortex-ng runs a coinjoin in which its taker output IS the funding output of this channel
  (vortex-ng ADR 0004: the taker opens and signs last). Public channels are regular (`wsh0` pit), since
  LND's taproot channels are private only.
- Status lines: `{"intent_id", "state": "accepted" | "pending_channel" | "broadcast" | "failed",
  "funding_outpoint"?: "txid:vout", "reason"?: "..."}`. `broadcast` carries the outpoint bipnip reports
  to the counterparty.
- On `failed` or a passed deadline, the executor has cancelled its pending channel; bipnip may retry
  with `PlainNode`.

## `RingIntent`: a whole ring in one transaction

```json
{
  "type": "ring_intent",
  "version": 1,
  "ring_id": "<order id>",
  "members": [{"node": "<66 hex>", "addr": "<host:port>"}],
  "my_index": 0,
  "capacity_sat": 2000000,
  "announce": true,
  "deadline_unix": 1762905600
}
```

Every member's agent sends the same `RingIntent`; the executors build one transaction holding every edge's
funding output, each member funding its own outgoing channel. This needs a vortex-ng specification
extending JoinMarket-NG's JMP-0014 rings (private taproot channels among makers) to regular, announced
channels; that specification is vortex-ng's to write, and the project owner reads it before anything is
posted upstream.

## Security

- Amounts and peers in an intent are checked by the agent's policy before the request is sent; vortex-ngd
  re-checks them against its own limits.
- The socket carries no keys, macaroons or preimages.
