# M0 Lightning spike: results (2026-11-10)

Run: `kata-run --net -- bash spikes/ln-m0/run.sh` on the agentrun Kata runner, regtest, Bitcoin Core 30.4,
LND v0.21.5-beta (two nodes), ldk-node 0.7.1, Core Lightning v26.06.10 (prebuilt). `check` verified everything
independently of the nodes. **10 checks, 0 failed.**

| question (ADR) | result |
|---|---|
| Do LND and ldk-node `signmessage` signatures recover to the node key in `bipnip-core`? (0004) | **yes**: two LND nodes and one ldk-node; now permanent vectors in `bipnip-core` (`real_node_signatures_recover`) |
| Do node-signed binding statements verify? (0004) | **yes**: LND-signed `nostr-node-binding:v1:bind:…` statements (`real_node_binding_statements_verify`) |
| Can a key held inside LND sign Nostr events without leaving the node? (0004, node-derived key) | **yes**: `walletrpc DeriveKey` (family 1735, index 0; deterministic across calls) + `signrpc SignMessage` with `schnorr_sig: true` over the NIP-01 serialization gives a valid Nostr event signature (rust-nostr `Event::verify`) |
| Hold invoice on a hash the payer chose, LND (0003) | **yes**: `addholdinvoice <payer's hash>`; the payer's HTLC is held (ACCEPTED) without the preimage; `settleinvoice <payer's preimage>` settles it. A brand-new channel refused the first attempt (`INSUFFICIENT_BALANCE`) for about 30 s, so the lease flow must wait for the channel to be usable on both sides before paying |
| Hold invoice on a hash the payer chose, LDK (0003) | **yes**: ldk-node `receive_for_hash` → `PaymentClaimable` → `claim_for_hash(payer's preimage)` → `PaymentReceived` |
| ldk-node accepting a channel from LND | only **unannounced** channels, unless ldk-node announces itself (alias + addresses); the lease flow must set announce flags accordingly |
| Core Lightning | **not run**: the prebuilt binary needs glibc 2.38 (the VM has older). Its signing format (`zbase`) is covered by Core Lightning's corpus in `bipnip-core`; a real CLN run moves to M1's CLN backend (booty's eltor-lab regtest has CLN v26.06.10) together with the hold-invoice plugin choice |
| Eclair | not in this spike (hex signature conversion is unit-tested in `bipnip-core`) |
