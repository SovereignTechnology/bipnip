# bipnip

A Nostr network for Lightning node runners. bipnip is a fork of [Mostro](https://mostro.network) (the
Nostr-native peer-to-peer Lightning exchange) that keeps all of Mostro and adds:

- **Liquidity leases**: buy or sell an inbound channel of a set size and duration. The buyer keeps the
  payment preimage until its own node has verified the channel, so no instance or seller can take the
  payment without opening it.
- **Liquidity rings**: 2–5 node runners each open a channel to the next, with optional bonds.
- **Node-bound identities**: a Nostr key and a Lightning node key bind each other with signatures anyone
  can verify offline (`docs/nips/node-binding.md`).

Everything travels over Nostr relays. Anyone who runs a node can run an instance; instances coordinate
leases and rings but never hold lease money.

Status: **M2, the trust layer and the web client's Liquidity tab (W1) done on regtest; hardening for
real funds in progress**. M0 (forks,
specs, spikes) is done. The LND backend passes its
conformance run against real nodes; `bipnip-agent` can create a mnemonic, bind a node and publish the
binding. A whole lease now runs end to end on regtest: a seller's agent offers liquidity on an instance,
a buyer's agent takes the order, pays the seller's hold invoice, checks the channel on its own node and
releases the preimage, between real LND nodes and through a real `bipnipd`, which never sees the preimage.
When part of the fee is due at the end, the seller bills it once the channel is as old as the lease is
long, and the buyer pays if its node still has the channel. The instance side is in `bipnipd`. Rings work
the same way: a creator proposes a ring, others take its slots, every member checks the roster and signs
it, and each opens one channel to the next and confirms the one opened to it. A ring can ask every member
for a bond, locked at the instance's node before anyone opens and paid on to the member that was let down
when a channel is missing. A lease offer can ask its buyer for a bond the same way, paid on to the seller
if the buyer walks away from an opened channel. Bonds are refused on mainnet until their code is read
(`bipnipd` #9). The
agent also runs as a daemon (`bipnip-agent run`) that carries every lease and ring on across restarts,
answers the `bipnip` command line on a unix socket, and holds each request to spending limits you set.
Three kinds of node are supported: **LND** (everything), **Core Lightning** with the `hold` plugin
(everything an agent does), and **LDK Server** (buying leases and joining rings; it cannot sell a lease yet,
`docs/decisions/0006-lightning-backend.md` says why). Leases are watched to their end, both parties write
up signed outcomes, and `bipnip record` counts a node's record from them. A web page (the `bipnip-web`
client's Liquidity tab, served by `bipnip-webhost`) can be paired with the agent and shows the book, the
sellers' records and the agent's runs; what it asks for waits for `bipnip approve`
(`itest/ln/run-web.sh` runs it in a real browser against real nodes). Nothing here is ready for real
funds.

## Try the agent (regtest or signet only)

Build with `cargo build --release -p bipnip-agent --features lnd` (needs `protoc`), then write a config:

```toml
network = "regtest"
data_dir = "/home/me/.bipnip"
relays = ["wss://relay.example"]

[lnd]
address = "https://127.0.0.1:10009"
tls_cert = "/home/me/.lnd/tls.cert"
macaroon = "/home/me/.lnd/data/chain/bitcoin/regtest/admin.macaroon"
```

```sh
bipnip-agent --config agent.toml init              # new mnemonic file, mode 600; back it up
bipnip-agent --config agent.toml whoami
bipnip-agent --config agent.toml bind --publish    # the node signs; the event goes to the relays
bipnip-agent --config agent.toml verify-binding <node pubkey>
```

Publishing a binding links your node's public key to a Nostr key for everyone to see.

The node is one of three sections of the config, and a build feature each (`--features lnd`, `cln`, `ldk`):

```toml
[lnd]                               # LND: gRPC, its TLS certificate, a macaroon
address = "https://127.0.0.1:10009"
tls_cert = "/home/me/.lnd/tls.cert"
macaroon = "/home/me/.lnd/bipnip.macaroon"

[cln]                               # Core Lightning: its JSON-RPC socket (the whole node; no narrower key)
rpc_socket = "/home/me/.lightning/bitcoin/lightning-rpc"   # selling needs the `hold` plugin

[ldk]                               # LDK Server: gRPC, its TLS certificate, a macaroon file
address = "127.0.0.1:3536"
tls_cert = "/home/me/.ldk-server/tls.crt"
macaroon = "/home/me/.ldk-server/bitcoin/macaroons/bipnip.macaroon"
```

### Leases

Add the instance to the config (its Nostr key, and the proof of work it asks for, if any):

```toml
[instance]
pubkey = "npub1…"
pow = 0
```

```sh
# Seller: offer 0.5 to 1 million sat channels for 144 blocks, and see the lease through.
bipnip-agent --config agent.toml lease offer --min-capacity 500000 --max-capacity 1000000 \
    --duration 144 --fee-base 1000 --fee-ppm 2000 --feerate 2

# Buyer: take an order the instance published, at a capacity inside its range.
bipnip-agent --config agent.toml lease take <order id> --capacity 600000 --node-address host:9735

bipnip-agent --config agent.toml lease list      # the stored leases and where they stand
bipnip-agent --config agent.toml lease resume    # carry on after a restart
```

A seller can ask its buyer for a bond with `lease offer … --bond 20000` (on an instance that takes bonds);
a buyer's agent takes such an order only up to its `[policy] max_bond_sat` (see Rings below).

Both commands run until the lease is active, when all of the fee was upfront (the default), or until it is
paid in full at the end of the lease when `--upfront` is below 100. The buyer's agent keeps the lease's preimage in an
owner-only file under `data_dir/leases` from before it sends the take, so a restart does not lose it.

An active lease is then watched by the agent's daemon (`bipnip-agent run`, below): both sides look at the leased
channel on their own node until it has as many confirmations as the lease is long, or is gone. The lease ends
`complete`, or `closed` when the channel went first. The buyer pays one completion invoice per lease, and pays it
when its own node saw the lease through, even if the channel is gone by the time the bill arrives; a seller whose
invoice expired unpaid bills again for up to a week.

### Rings

```sh
# Propose a ring of three 1 million sat channels and take its first slot.
bipnip-agent --config agent.toml ring create --capacity 1000000 --slots 3 --feerate 2 \
    --node-address host:9735

# Take a slot of a ring the instance published.
bipnip-agent --config agent.toml ring join <order id> --feerate 2 --node-address host:9735

bipnip-agent --config agent.toml ring list
bipnip-agent --config agent.toml ring resume
```

Each member opens one channel, to the next member, and only after every member has signed the same
roster. A member that signs and never opens costs the others the channel they opened, which stands.

A ring can ask for a bond (`ring create … --bond 20000`, on an instance that takes bonds): every member
locks that much at the instance's node before anyone opens. It comes back when your channel is there. If
the member that should have opened to you never does, its bond is paid on to you. Your agent joins such a
ring only up to a limit you set, and pays a bond only to the node the instance's own binding names:

```toml
[policy]
max_bond_sat = 50000    # 0, the default: join no ring that asks for a bond
```

A bond is money the instance holds until the ring ends, and the instance decides whose bond is taken
(`docs/nips/liquidity-rings.md` says on what evidence, and what that leaves you trusting).

### The daemon and the `bipnip` command line

The commands above each see one lease or ring through and end. To keep offers up, run several things at
once and lose nothing over a restart, run the agent as a daemon and talk to it with `bipnip`:

```sh
bipnip-agent --config agent.toml run &          # holds the keys and the node; listens on
                                                 # <data_dir>/agent.sock, same user only
export BIPNIP_SOCKET=/home/me/.bipnip/agent.sock
bipnip status
bipnip orders                                    # the open orders of your instances
bipnip lease take <order id> --capacity 600000 --node-address host:9735
bipnip lease list
bipnip watch                                     # every change of every run, as it happens
```

The daemon moves the node's money only within limits you set. Every limit defaults to 0, so a fresh
agent parks every request until you approve it:

```toml
[[instance]]                     # may appear more than once; pow is read from the instance
pubkey = "npub1…"

[policy]
max_fee_per_lease_sat = 5000     # a buyer's whole fee for one lease
max_fee_per_day_sat = 20000      # all leases taken in a day
max_capacity_per_day_sat = 3000000   # channels this agent opens in a day: leases sold, rings
max_bond_sat = 50000
max_runs = 4                     # leases and rings under way at once
allowed_peers = []               # node keys; empty means any
```

```sh
bipnip approvals                 # what is parked, and which limit it is over
bipnip approve <approval>        # shows what it would do and asks; --yes in scripts
bipnip deny <approval>
```

A web page (the `bipnip-web` client through `bipnip-webhost`) reaches the daemon through a second socket,
opened by a `[web]` section, once paired: `bipnip pair open`, then `bipnip pair approve <code the page
shows> [--scope trade]`; `bipnip sessions` and `bipnip revoke <session>` manage them. Every run a page asks
for is parked until `bipnip approve`. `docs/decisions/0011-key-roles-pairing-and-session-keys.md` says what
a page can and cannot do.

A run the daemon starts can pay and open only what it was granted: a payment or a channel beyond that is
refused before it reaches the node. While the daemon runs, the one-shot `lease` and `ring` commands are
refused (one process at a time drives the runs of a data directory). `docs/agent-api.md` is the socket's
protocol; `docs/decisions/0010-agent-daemon-and-local-api.md` says what the socket and the approvals do
and do not protect against.

## Repositories

| repo | what | license |
|---|---|---|
| [`bipnip`](https://github.com/SovereignTechnology/bipnip) (this one) | `bipnip-agent` (runs next to your node, holds your keys), the `bipnip` CLI, `bipnip-ln` (LND, Core Lightning, LDK backends), later the web client; project docs | MIT OR Apache-2.0 |
| [`bipnipd`](https://github.com/SovereignTechnology/bipnipd) | the instance daemon, a fork of Mostro's `mostrod` | GPL-3.0-or-later |
| [`bipnip-core`](https://github.com/SovereignTechnology/bipnip-core) | protocol types: Mostro's, plus liquidity and node binding; a fork of `mostro-core` | MIT |
| [`bipnip-web`](https://github.com/SovereignTechnology/bipnip-web) | the web client: a fork of the [RoboSats](https://github.com/RoboSats/robosats) UI, served by your own agent | AGPL-3.0 |

Upstream Mostro apps keep working against a `bipnipd` instance for fiat trades.

## Docs

- Decisions: `docs/decisions/` (fork and licenses, roles, lease payment, node binding, event kinds,
  Lightning backends, rings, reputation, web-client security, the agent daemon and its local API, key
  roles and pairing web pages)
- `docs/agent-api.md`: the daemon's unix-socket protocol, which `bipnip` speaks
- Specs (drafts): `docs/nips/node-binding.md`, `docs/nips/liquidity-leases.md`,
  `docs/nips/liquidity-rings.md`
- `docs/executor-api.md`: how channel opens are handed to an executor (the node itself, or
  [vortex-ng](https://github.com/SovereignTechnology/vortex-ng) coinjoins)
- `docs/security/inherited-issues.md`, `docs/upstream-pins.md`

## Roadmap

M0 forks, specs and spikes · M1 Lightning backends (LND, CLN, LDK), node binding, the agent and CLI ·
M2 leases and rings · M3 vortex-ng as an executor · M4 discovery and reputation across instances ·
M5 the web client (the RoboSats UI fork: pure RoboSats first, then wired to bipnip; a Rust/WebAssembly
rewrite later) · M6 upstream contributions.

## Thanks

To the Mostro developers, whose daemon and protocol bipnip builds on, and to Amboss Magma,
LightningNetwork.plus and publsp for showing what node runners need.
