NIP-XX
======

Lightning Node ↔ Nostr Key Binding
----------------------------------

`draft` `optional`

Status: bipnip draft, 2026-11-10. Not posted upstream. Kind number provisional.

This NIP defines an event that binds a Nostr public key to a Lightning node's identity key with a
signature from **each** key, so anyone can verify, without a Lightning node, that a Lightning node and a
Nostr key belong to the same operator.

## Motivation

Lightning liquidity markets, routing communities and LSP directories want to know which Nostr key speaks
for which node. Today a node key can be claimed in a profile or an app event by anyone. Every Lightning
implementation can already sign arbitrary messages with its node key, in one shared format, so a
two-way binding needs no new cryptography.

## Node signature format

The node signs with its existing message-signing RPC (LND `signmessage`, Core Lightning `signmessage`
field `zbase`, LDK `util::message_signing::sign`, ldk-node `sign_message`):

```text
signature = zbase32( header ‖ r ‖ s )
header    = 31 + recovery_id          (one byte, 31..=34: compressed key)
(r, s)    = ECDSA over sha256(sha256("Lightning Signed Message:" ‖ statement))
```

zbase32 uses the alphabet `ybndrfg8ejkmcpqxot1uwisza345h769`, most significant bit first, no padding; a
65-byte signature is 104 characters. Eclair returns the same 65 bytes as hex; clients convert it to zbase32
before publishing. Verifiers MUST recover the public key from the signature and compare it with the node
key in the statement. They MUST NOT depend on a Lightning node's own `verifymessage`, which only accepts
keys present in that node's graph.

## Statement

The node signs exactly this ASCII text:

```text
nostr-node-binding:v1:<action>:<network>:<nostr-pubkey>:<node-pubkey>:<created_at>
```

| field | value |
|---|---|
| `action` | `bind` or `revoke` |
| `network` | `bitcoin`, `testnet`, `testnet4`, `signet` or `regtest` |
| `nostr-pubkey` | the Nostr public key, 64 lower-case hex characters |
| `node-pubkey` | the node's compressed identity key, 66 lower-case hex characters |
| `created_at` | unix seconds, decimal, no leading zeros |

A statement that is not byte-identical to this form for its parsed values MUST be rejected.

## Event

```json
{
  "kind": 31735,
  "pubkey": "<nostr-pubkey>",
  "created_at": <created_at of the statement>,
  "content": "",
  "tags": [
    ["d", "<node-pubkey>"],
    ["node", "<node-pubkey>"],
    ["network", "<network>"],
    ["statement", "<statement text>"],
    ["node_sig", "<zbase32 node signature over the statement>"]
  ]
}
```

A client MUST reject the event unless all of the following hold:
1. The event id and signature are valid (NIP-01).
2. Each of `d`, `node`, `network`, `statement`, `node_sig` appears exactly once with exactly one value.
3. The statement parses, its `node-pubkey` equals `d` and `node`, its `network` equals `network`, its
   `nostr-pubkey` equals the event's `pubkey`, and its `created_at` equals the event's `created_at`.
4. The node signature recovers to the statement's `node-pubkey`.

## Resolution and revocation

The node key is the authority over its bindings. Among the valid events for one node, the statement with
the greatest `created_at` decides; at equal `created_at` a `revoke` wins over a `bind`. Clients MUST ignore
statements dated more than 600 seconds ahead of their own clock: otherwise a statement dated far in the
future would outrank every later revocation. A node binds one
Nostr key at a time: a newer `bind` to another key replaces the older one.

To revoke, the node signs a `revoke` statement and the Nostr key publishes it under the same `d`. Because
the node signature is checked, a party holding only a stolen Nostr key cannot publish a revocation of a
different binding, and a replayed older `bind` loses to the newer `revoke`. A node that has lost its
Nostr key can still revoke: any Nostr key may publish a revocation whose statement names the old key, and
clients MUST accept a valid node-signed `revoke` regardless of which key published it.

## Signature malleability

An ECDSA signature `(r, s)` has a twin `(r, n − s)` with the other recovery id that recovers the same key.
Both are valid node signatures over the same statement, so two binding events can differ only in
`node_sig`. Clients MUST NOT treat the signature text as an identifier; the statement decides.

## Privacy

Publishing a binding links the node, its channels and on-chain history to the Nostr identity. Operators of
unannounced or Tor-only nodes MAY send the event only to counterparties inside encrypted messages instead
of publishing it. Applications SHOULD NOT reuse a node-bound key for activity unrelated to the node.

## Optional: a Nostr key derived inside the node

A node MAY derive the Nostr key from its own seed, so the key is recoverable with the node and needs no
separate backup: for example Core Lightning `makesecret` with a fixed label, or LND's signer with a
dedicated key locator. The binding event is the same; the derivation is not visible to verifiers.

## Test vectors

`bipnip-core` (`src/node_binding.rs`) checks its implementation against rust-lightning's message-signing
vectors and Core Lightning's lnd-produced corpus (`tests/test_misc.py`), and round-trips binding events
signed by both keys.
