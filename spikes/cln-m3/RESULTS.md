# Phase 3 Core Lightning spike: results (2026-11-17)

Run: `kata-run --net -- bash spikes/cln-m3/run.sh` on the agentrun Kata runner, regtest, Bitcoin Core 30.4,
one LND v0.21.5-beta node (alice) and one Core Lightning v26.06.10 node (carol) with the `hold` plugin
(BoltzExchange/hold v0.3.3), driven through `lightning-cli`. The script asserts little: these are the node's
and the plugin's own answers.

## The plugin: picked by what it can do

Core Lightning has no hold invoices of its own. Two plugins add them:

- **daywalker90/holdinvoice: ruled out before any test.** It cannot make an invoice on a payment hash the
  caller chose (it takes a preimage, so the node knows the preimage from the start), it settles by hash, and
  it has been archived since 2025-12-08 with a pointer to `hold`. A lease needs the buyer to keep the
  preimage.
- **BoltzExchange/hold (MIT): does everything a lease's seller needs**, over Core Lightning's own JSON-RPC,
  and passed every question below on v26.06.10 (it does not state which Core Lightning versions it supports).

## What it means for the backend

Core Lightning with `hold` can be a **seller, a buyer and a ring member**. Compared with LND:

- the lock time of a hold invoice is the plugin's (80 blocks) unless the invoice is made through the plugin's
  own gRPC interface; through the RPC it cannot be set, nor can the expiry (one hour) or the description;
- the plugin gives a held payment back 3 blocks before it expires (`hold-expiry-deadline`), so with 80 blocks
  a seller has about 77: enough for a lease that waits for a few confirmations.

## Answers

| question | answer |
|---|---|
| 1. Does the prebuilt Core Lightning run in the VM? | **Yes**: the Ubuntu 22.04 build (glibc 2.35) on the VM's Debian 12 (glibc 2.36), with `libpq5 libsqlite3-0 libsodium23`. The 24.04 build did not (M0 spike) |
| 1. Does `hold` v0.3.3 load on v26.06.10? | **Yes**, as `--plugin=<path>`; it keeps `hold/hold.sqlite3` and gRPC certificates under `<lightning-dir>/regtest/hold/`. `cln-grpc` is on by default too |
| 2. `holdinvoice <payment_hash> <amount_msat>` | Answers `{"bolt11": …}`: payee the node, the caller's hash, `min_final_cltv_expiry` **80**, expiry **3600 s**, empty description. A second invoice on the same hash is refused |
| 2. Its state: `listholdinvoices <payment_hash>` | `unpaid` → `accepted` once LND's HTLC is in (with each HTLC's `cltv_expiry`: 196 at height 113) → `paid` after `settleholdinvoice <preimage>`; the payer sees the preimage. Settling again answers `{}` |
| 2. Held, then `cancelholdinvoice <payment_hash>` | `cancelled`; the payer's payment fails at once. Cancelling again answers `{}` |
| 3. Cancelled before anyone paid, then paid | **Stays `cancelled`, and the payment is refused**: a real cancel |
| 3. Settle with nothing held | Refused: `no HTLCs to settle` |
| 3. A hash nobody made an invoice for | `listholdinvoices` answers an empty list; `cancelholdinvoice` answers `invoice not found` |
| 4. Core Lightning restarted while a payment is held | **Still `accepted` after the restart**, and it settles |
| 5. Paying a plain invoice (`pay`) | `complete` with the preimage; `listpays null <payment_hash>` shows it. **Paying it again answers the same `complete`**, no second payment |
| 5. Paying a hold invoice | `pay` blocks while the payee holds; `listpays` shows `pending`. A second `pay` meanwhile is refused (`… still in progress, cannot retry before that completes`). Once the payee settles: `complete` with the preimage |
| 5. A hold invoice the payee cancels | `listpays` shows `failed`; a hash never paid shows an empty list |
| 6. An announced channel LND → Core Lightning | Works; `listpeerchannels <id>`: `opener: remote`, `private: false`, `funding_txid`, `funding_outnum`, `total_msat`, `state: CHANNELD_NORMAL` |
| 6. An announced channel Core Lightning → LND | `fundchannel <id> <sat> normal true` answers the funding `txid` at once; `CHANNELD_AWAITING_LOCKIN` until confirmed, then `CHANNELD_NORMAL` |
| 6. The graph | `listchannels null <node id>` lists a node's public channels with `short_channel_id`, `amount_msat` and `public`; `listnodes <id>` its announcement. No funding outpoint: a short channel id (`block x tx x output`) names the transaction by position, not by id |
| `signmessage` | `signature`, `recid` and `zbase`, the form LND gives |

One thing seen on the way: Core Lightning follows the chain by polling. Right after blocks were mined its
`pay` was first refused by LND for a stale height ("their blockheight was 125, we're at 119") and went through
on its own retry. A backend must expect a short lag after a block.

Not answered here (the plugin's gRPC interface is needed): a hold invoice with a chosen lock time, expiry and
description.
