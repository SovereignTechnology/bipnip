# Phase 3 LDK spike: results (2026-11-17)

Run: `kata-run --net -- bash spikes/ldk-m3/run.sh` on the agentrun Kata runner, regtest, Bitcoin Core 30.4,
one LND v0.21.5-beta node (alice) and one LDK Server (`ldk-server`, upstream revision
`d7e7a2e708664af922c53c786ad6a5df622f5dd8`, 2026-11-12; dave), driven call by call through
`ldk-server-client`. Two runs; the second added the questions the first raised. The script asserts
little: these are the server's own answers.

## What it means for the backend

An LDK Server works as a **payer, a channel opener and a signer** with nothing in the way. So an LDK
node can buy a lease and sit in a ring.

It **cannot yet hold a payment safely for a seller**, for three separate reasons (questions 1 to 4 below).
`bipnip-ln`'s LDK backend therefore answers "not supported" for hold invoices, and an agent on such a node
offers no lease. Two additions to LDK Server would lift this:

1. a lock time (`min_final_cltv_expiry_delta`) on `Bolt11ReceiveForHash`;
2. a lookup of a held payment by its payment hash that says until which block it can be claimed (or a
   payment record that changes when the node gives the payment back).

## Answers

| question | answer |
|---|---|
| 1. A hold invoice (`Bolt11ReceiveForHash`): its lock time | `min_final_cltv_expiry_delta` **42**, fixed: the request has no field for it |
| 1. Is there a payment record before anyone pays? | **No.** `GetPaymentDetails(<payment hash>)` answers `payment: None`, and `ListPayments` has no entry. An invoice nobody paid and a hash nobody made an invoice for look the same |
| 2. When the payer's HTLC arrives | A record appears under a **random payment id, not the hash** (`kind.bolt11.hash` carries the hash), status `Pending`. `PaymentClaimable` carries that id, the amount and `claim_deadline` |
| 2. How long until the claim deadline | **6 blocks** (height 113, deadline 119; the LND payer's HTLC expired at 158, so the deadline is 39 blocks before expiry) |
| 3. Claim with the payer's preimage | `Bolt11ClaimForId(<event id>, preimage)` works; the payer sees the preimage. Claiming again answers OK |
| 3. Fail while held | `Bolt11FailForId(<event id>)` works; the payer's payment fails at once. Failing again answers OK |
| 3. Fail or claim by the payment hash, or before anyone paid | **`InvalidRequestError: The given payment id is invalid.`** An unpaid hold invoice cannot be cancelled; it can only be failed once someone pays it |
| 4. A `PaymentClaimable` while nobody is subscribed | **Never seen again**: not on subscribing later, not after a server restart |
| 4. Can the payment still be found without the event? | **Yes, in the list**: `ListPayments` has the `Pending` inbound record with the hash and its id |
| 4. Can it be claimed by the id the list gave? | **Yes**, with the event never seen, and also after a server restart |
| 4. A payment held past its deadline with nobody acting | The node gives it back (the payer's payment fails), **and the record still says `Pending`**. The list cannot tell a payment that is held from one that was given back |
| 5. An outbound payment's id | **Its payment hash.** `GetPaymentDetails(<hash>)`: `Pending` while the payee holds it, `Succeeded` with the preimage once settled, `Failed` once cancelled; `payment: None` for a hash never paid |
| 5. Sending the same invoice again | While pending or once paid: **`LightningError: A payment with the given hash has already been initiated.`** After it failed: a new send works |
| 6. An announced channel LND → LDK Server | Works when the server has an alias and an announcement address (and on-chain funds for its anchor reserve). It wants **6 confirmations** before the channel is usable |
| 6. An announced channel LDK Server → LND | Works. `OpenChannel` answers only a `user_channel_id`; the funding outpoint shows in `ListChannels` about two seconds later. Usable after 1 confirmation on its side |
| 6. The graph | `GraphGetNode` lists a node's channels by **short channel id only** (no funding outpoint); an unknown node is an `InvalidRequestError`, not an empty answer |
| `SignMessage` | The zbase32 form LND gives (`bipnip-core` already recovers it; M0 spike) |
| `GetNodeInfo` | Node id, network, best block height; no "synced" flag, only the times of the last wallet syncs |

Also learned about running it: `ldk-server <config.toml>`; credentials at `<storage_dir>/tls.crt` and
`<storage_dir>/<network>/macaroons/admin.macaroon` (hex text); a debug build from source takes about three
minutes in the VM and needs `cmake` and `clang`; `ListPayments` returned everything in one page at this size.

## Why the three reasons each matter on their own

- **Six blocks.** A seller must open a channel, see it confirmed as many times as the lease asks (three by
  default), get the preimage from the buyer and claim, all before the deadline. On regtest that is a matter of
  mining; on mainnet it is an hour that one slow block breaks.
- **No lookup.** The agent's seller opens the channel only once its own node says the payment is held. The
  only signals are an event that is lost if the agent is not listening at that moment, and a list entry that
  never changes back. A seller that trusted the list could open a channel for a payment already given back.
- **No cancel before payment.** A seller that withdraws cannot make its invoice unpayable; it must be running
  when a late payment arrives in order to fail it.
