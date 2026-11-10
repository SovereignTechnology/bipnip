# 4. Node ↔ Nostr key binding, and keeping fiat trades unlinked

Date: 2026-11-10

## Status

Accepted (normative text: `docs/nips/node-binding.md`; code: `bipnip-core` `node_binding`)

## Context

The project owner wants Nostr identities bound to Lightning node keys so "the lightning node does have to
be involved in the social aspect", via "signed attestation + optional node-derived key". No NIP binds a
node key to a Nostr key (checked 2026-11-10). Mostro's instance info event (kind 38385) self-asserts
`lnd_node_pubkey` with no proof from the node.

Every implementation signs messages the same way: `zbase32(31 + recid ‖ compact_sig(sha256d("Lightning
Signed Message:" ‖ msg)))` (LND, Core Lightning's `zbase` field, LDK, ldk-node); Eclair returns the same
65 bytes as hex. A node's own `verifymessage` only accepts keys already in its graph.

## Decision

- **A binding is signed by both keys.** The node signs a canonical statement
  `nostr-node-binding:v1:<bind|revoke>:<network>:<nostr pubkey hex>:<node pubkey hex>:<unix seconds>`;
  the Nostr key signs the addressable event that carries the statement and the node's signature
  (`d` = node key). Verifiers recover the node key from its signature themselves.
- **The node key is the authority.** For one node the newest node-signed statement wins; a revocation
  is a node-signed `revoke` statement and wins ties. A stolen Nostr key cannot keep a binding alive, and
  a replayed old binding loses to the newer revocation. One node binds one Nostr key at a time.
- **Optional node-derived key:** the Nostr key is derived inside the node so it is recoverable from the
  node's seed and needs no separate backup: Core Lightning `makesecret`; LND's signer with a dedicated key
  locator and Schnorr `SignMessage` (the M0 spike must show its output is a valid Nostr signature
  without exporting the key). LDK and Eclair use the attestation only.
- **Privacy split:** the node-bound key is the **liquidity identity**. Fiat trades keep Mostro's
  identity key (index 0 of the NIP-06 path) or Mostro's full-privacy mode and are never linked to the
  node by default: a public node key tied to fiat trades would deanonymise the trader.
- Unannounced or Tor-only nodes may send their binding only to counterparties (inside encrypted
  messages) instead of publishing it.
- **Instances publish a proven binding** next to kind 38385 instead of trusting the self-asserted field.

## Consequences

- Binding a node publishes its key next to a Nostr identity. For the project owner's own mainnet node
  that is a decision he makes himself, later.
- Verification needs no Lightning node and works in the browser (the code is pure and wasm-clean).
- A node key compromise is fatal to the binding (as it is to the node).
