# 2. Two roles: instances coordinate, agents hold the keys

Date: 2026-11-10

## Status

Accepted

## Context

In Mostro an *instance* (one operator: one Nostr key, one LND node, one SQLite database) publishes the
order book, holds every escrow hold invoice and its preimage, and runs disputes; *clients* (the app, the
TUI, the CLI) hold the user's NIP-06 mnemonic and talk to one instance at a time. Discovery across
instances does not exist (app issue #136).

bipnip's liquidity trades differ from fiat trades in one decisive way: the thing bought, a Lightning
channel, can be checked by the buyer's own node. Fiat cannot be checked by software, so fiat escrow needs
a party trusted to judge disputes; liquidity does not. The project owner chose instance-coordinated
liquidity, with instances that never hold lease money.

## Decision

- **Instance (`bipnipd`).** Publishes order books (fiat as Mostro does; liquidity under its own kind),
  rate-limits with Mostro's NIP-13 proof of work, verifies node bindings against its own node's graph,
  keeps per-instance ratings, runs disputes and solvers, holds fiat escrow exactly as Mostro does, and
  coordinates leases and rings. **For leases and rings it never holds the payment or its preimage**
  (ADR 0003); it may hold short-lived bonds where a trade asks for them.
- **Agent (`bipnip-agent`).** Runs next to its owner's node and holds the NIP-06 mnemonic (identity key
  and per-trade keys, Mostro's derivation `m/44'/1237'/38383'/0/N`) and the node credentials. It is a
  Mostro client for fiat trades and buyer, seller or ring member for liquidity trades. It follows many
  instances at once and merges their order books. Without a node it still trades fiat, as Mostro clients
  do (the user pays and receives with any wallet).
- **Anyone who runs a node can run an instance.** The instance's own node binding (ADR 0004) is
  published in its kind 38385 info event, so its identity and track record are checkable.
- **The agent is the only component with keys or node access**; the CLI and the later web client are
  its clients (ADR 0009).

## Consequences

- No single central party: an instance can censor or go offline, and agents move to another one;
  for liquidity it cannot take money.
- Fiat trades keep Mostro's trust model (the instance can settle and not pay, and solvers decide
  disputes); bipnip improves only its accountability (proven node identity, fixed security issues).
- Two binaries to release and secure; the agent carries the larger attack surface (keys + node).
