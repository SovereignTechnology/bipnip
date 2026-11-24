#!/usr/bin/env bash
# The soak: agents on LND, Core Lightning and LDK trade leases, rings and fiat through a real bipnipd
# for hours or days, on a private chain whose blocks come at random, while processes are killed,
# relays go away and the chain reorganises; an auditor checks what must always hold. Inside a VM:
#
#   kata-run --cwd ~/Projects/bipnip --net --timeout 10800 -- \
#     env SOAK_HOURS=1 SOAK_BLOCK_SECS=30 bash itest/soak/run-soak.sh          (the smoke)
#
# and for days in the booty lab (sovtech/booty bipnip-soak/). SOAK_CHAIN is regtest (default) or
# signet. Tunables (digits only):
#   SOAK_HOURS (default 1), SOAK_BLOCK_SECS (mean block interval, default 600),
#   SOAK_AUDIT_SECS (default 600), SOAK_FAULT_SECS (mean time between faults, default 1800),
#   SOAK_SEED (default: the time). BIPNIPD_REV picks the instance.
#
# The soak mines its own blocks, at random intervals, from a wallet in this VM. SOAK_CHAIN=signet makes
# it a signet whose challenge is OP_TRUE (51: no signature needed), but LND v0.21.5 refuses a custom
# signet with a bitcoind backend ("expected network Unknown BitcoinNet (3178222164), got SigNet"), so
# regtest is the default: the same timing and faults, on regtest's rules. Nothing reaches another network.
# Every key and credential is a throwaway value inside the VM.

set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
LN=$HERE/../ln
W=${SOAK_WORK:-/tmp/bipnip-soak}
rm -rf "$W"; mkdir -p "$W"/{dl,bin,btc,cln,soak}
log() { printf '[soak %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
fail() { log "FAIL: $*"; exit 1; }
jget() { python3 -c 'import json,sys
d=json.load(sys.stdin)
for k in sys.argv[1:]: d=d[k]
print(d)' "$@"; }
wait_until() { local secs=$1; shift; local t=0; until "$@" >/dev/null 2>&1; do sleep 1; t=$((t+1)); [ "$t" -lt "$secs" ] || return 1; done; }
digits() { [[ "$2" =~ ^[0-9]+$ ]] || fail "$1 must be digits"; }
SOAK_HOURS=${SOAK_HOURS:-1}; digits SOAK_HOURS "$SOAK_HOURS"
SOAK_BLOCK_SECS=${SOAK_BLOCK_SECS:-600}; digits SOAK_BLOCK_SECS "$SOAK_BLOCK_SECS"
SOAK_AUDIT_SECS=${SOAK_AUDIT_SECS:-600}; digits SOAK_AUDIT_SECS "$SOAK_AUDIT_SECS"
SOAK_FAULT_SECS=${SOAK_FAULT_SECS:-1800}; digits SOAK_FAULT_SECS "$SOAK_FAULT_SECS"
SOAK_SEED=${SOAK_SEED:-$(date +%s)}; digits SOAK_SEED "$SOAK_SEED"

BTC_V=30.4
BTC_SHA=051a18c595d9a27df23cb7187ff9fb343e90943cb142af751e12787806500c3d
LND_V=v0.21.5-beta
LND_SHA=2127d4794ea9cbd98e762ba16283615158743646d2d6d2a5bf88664dd6255ce7
CLN_V=v26.06.10
CLN_SHA=e1e0a7e89b55060e2470d8568ba535a3124d6823c628def548cadeef4279c491
HOLD_V=v0.3.3
HOLD_SHA=f9d916430fb70a3d75089f2d1b88d5d71d4987ef79933035978c1f44aab84be5
LDK_SERVER_REV=d7e7a2e708664af922c53c786ad6a5df622f5dd8
# The instance: H1's integration branch (the v0.20.3 merge and every fix in review).
BIPNIPD_REV=${BIPNIPD_REV:?set BIPNIPD_REV to the bipnipd commit to soak}
# mostro-cli v0.17.1, for the fiat trades (as run-fiat.sh).
MCLI_REV=0665895f0a9806756252bd34a0572416f7c13e61
fetch() { curl -fsSL --retry 3 -o "$3" "$1"; echo "$2  $3" | sha256sum -c --quiet; }
NET=${SOAK_CHAIN:-regtest}
case "$NET" in
  regtest) RPCPORT=18443; LND_NET=(--bitcoin.regtest); CHALLENGE="" ;;
  signet) RPCPORT=38332; LND_NET=(--bitcoin.signet --bitcoin.signetchallenge=51); CHALLENGE="signetchallenge=51" ;;
  *) fail "SOAK_CHAIN must be regtest or signet" ;;
esac

log "tools"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq protobuf-compiler python3 curl xz-utils ca-certificates git cmake clang pkg-config \
  libssl-dev build-essential libpq5 libsqlite3-0 libsodium23 zlib1g >/dev/null

log "downloads (sha256-pinned)"
fetch "https://bitcoincore.org/bin/bitcoin-core-$BTC_V/bitcoin-$BTC_V-x86_64-linux-gnu.tar.gz" "$BTC_SHA" "$W/dl/btc.tgz"
fetch "https://github.com/lightningnetwork/lnd/releases/download/$LND_V/lnd-linux-amd64-$LND_V.tar.gz" "$LND_SHA" "$W/dl/lnd.tgz"
fetch "https://github.com/ElementsProject/lightning/releases/download/$CLN_V/clightning-$CLN_V-Ubuntu-22.04-amd64.tar.xz" "$CLN_SHA" "$W/dl/cln.txz"
fetch "https://github.com/BoltzExchange/hold/releases/download/$HOLD_V/hold-linux-amd64.tar.gz" "$HOLD_SHA" "$W/dl/hold.tgz"
tar xzf "$W/dl/btc.tgz" -C "$W/dl"; cp "$W/dl/bitcoin-$BTC_V/bin/"{bitcoind,bitcoin-cli} "$W/bin/"
tar xzf "$W/dl/lnd.tgz" -C "$W/dl"; cp "$W/dl/lnd-linux-amd64-$LND_V/"{lnd,lncli} "$W/bin/"
tar xJf "$W/dl/cln.txz" -C "$W/cln"
tar xzf "$W/dl/hold.tgz" -C "$W/dl"
HOLD=$(find "$W/dl" -type f -name 'hold-linux-amd64' | sed -n 1p); chmod +x "$HOLD"
CLN_BIN=$(dirname "$(find "$W/cln" -type f -name lightningd | sed -n 1p)")
export PATH=$W/bin:$CLN_BIN:$PATH

log "builds: the instance (${BIPNIPD_REV:0:12}), LDK Server and mostro-cli beside the agent, the CLI and the helpers"
git clone -q https://github.com/SovereignTechnology/bipnipd "$W/bipnipd"
git -C "$W/bipnipd" checkout -q "$BIPNIPD_REV"
git clone -q https://github.com/lightningdevkit/ldk-server "$W/ldk-server"
git -C "$W/ldk-server" checkout -q "$LDK_SERVER_REV"
git clone -q https://github.com/MostroP2P/mostro-cli "$W/mostro-cli"
git -C "$W/mostro-cli" checkout -q "$MCLI_REV"
# Each tree may pin its own toolchain; let rustup install them one at a time (two installs of the
# same toolchain at once collide in its download directory).
for tree in "$W/bipnipd" "$W/ldk-server" "$W/mostro-cli" "$HERE/../.." "$LN"; do
  (cd "$tree" && cargo --version) > /dev/null || fail "no Rust toolchain for $tree"
done
cargo build --quiet --release --locked --manifest-path "$W/bipnipd/Cargo.toml" --bin mostrod --target-dir "$W/target-d" > "$W/build-d.log" 2>&1 &
BUILD_D=$!
(cd "$W/ldk-server" && cargo build --quiet --locked -p ldk-server --target-dir "$W/target-ldk") > "$W/build-ldk.log" 2>&1 &
BUILD_L=$!
(cd "$W/mostro-cli" && cargo build --quiet --release --locked --target-dir "$W/target-m") > "$W/build-m.log" 2>&1 &
BUILD_M=$!
cargo build --quiet --locked --release --manifest-path "$LN/Cargo.toml" --target-dir "$W/target"
cargo build --quiet --locked --release --manifest-path "$HERE/../../Cargo.toml" -p bipnip-agent --features lnd,cln,ldk --target-dir "$W/target-ws"
cargo build --quiet --locked --release --manifest-path "$HERE/../../Cargo.toml" -p bipnip-cli --target-dir "$W/target-ws"
wait "$BUILD_D" || { tail -30 "$W/build-d.log"; fail "the instance did not build"; }
wait "$BUILD_L" || { tail -30 "$W/build-ldk.log"; fail "LDK Server did not build"; }
wait "$BUILD_M" || { tail -30 "$W/build-m.log"; fail "mostro-cli did not build"; }
AGENT=$W/target-ws/release/bipnip-agent

log "bitcoind: a private $NET, no peers"
cat > "$W/btc/bitcoin.conf" <<CONF
$NET=1
server=1
fallbackfee=0.0002
[$NET]
$CHALLENGE
connect=0
listen=0
dnsseed=0
rpcuser=soak
rpcpassword=soak
rpcport=$RPCPORT
rpcbind=127.0.0.1
rpcallowip=127.0.0.1
zmqpubrawblock=tcp://127.0.0.1:28332
zmqpubrawtx=tcp://127.0.0.1:28333
CONF
bitcoind -datadir="$W/btc" -daemon
BCLI="bitcoin-cli -datadir=$W/btc"
wait_until 60 $BCLI getblockchaininfo
[ "$($BCLI getblockchaininfo | jget chain)" = "$NET" ] || fail "bitcoind is not on $NET"
$BCLI createwallet miner >/dev/null
MADDR=$($BCLI -rpcwallet=miner getnewaddress)
$BCLI generatetoaddress 1 "$MADDR" > "$W/first-block.out" 2>&1 \
  || { cat "$W/first-block.out"; fail "this $NET does not take a block mined here (generatetoaddress)"; }
$BCLI generatetoaddress 110 "$MADDR" >/dev/null
mine() { $BCLI generatetoaddress "$1" "$MADDR" >/dev/null; }

log "LND: alice and bob (agents' nodes), carol (the instance's node and the payment hub)"
declare -A RPC=([alice]=10009 [bob]=10010 [carol]=10011) REST=([alice]=8081 [bob]=8082 [carol]=8083) \
  P2P=([alice]=9735 [bob]=9736 [carol]=9737)
lnd_cmd() { # name: the command line, one word a line (also handed to the driver for restarts)
  printf '%s\n' lnd "--lnddir=$W/$1" "${LND_NET[@]}" --bitcoin.node=bitcoind \
    "--bitcoind.rpchost=127.0.0.1:$RPCPORT" --bitcoind.rpcuser=soak --bitcoind.rpcpass=soak \
    --bitcoind.zmqpubrawblock=tcp://127.0.0.1:28332 --bitcoind.zmqpubrawtx=tcp://127.0.0.1:28333 \
    --noseedbackup "--rpclisten=127.0.0.1:${RPC[$1]}" "--restlisten=127.0.0.1:${REST[$1]}" \
    "--listen=127.0.0.1:${P2P[$1]}" "--alias=$1" --trickledelay=1000
}
for n in alice bob carol; do
  mkdir -p "$W/$n"
  mapfile -t cmd < <(lnd_cmd "$n")
  "${cmd[@]}" > "$W/$n/lnd.log" 2>&1 &
done
lnc() { local n=$1; shift; lncli --lnddir="$W/$n" --network=$NET --rpcserver=127.0.0.1:${RPC[$n]} "$@"; }
synced() { [ "$(lnc "$1" getinfo | jget synced_to_chain)" = "True" ]; }
for n in alice bob carol; do wait_until 180 synced "$n" || { tail -20 "$W/$n/lnd.log"; fail "$n did not sync on $NET"; }; done
mac() { echo "$W/$1/data/chain/bitcoin/$NET/admin.macaroon"; }

log "Core Lightning with the hold plugin: chris (fee rates forced: a private chain has no estimates)"
CHRIS_P2P=9741
mkdir -p "$W/chris"
cln_cmd() {
  printf '%s\n' lightningd "--network=$NET" "--lightning-dir=$W/chris" --daemon "--log-file=$W/chris/log" \
    --bitcoin-rpcconnect=127.0.0.1 "--bitcoin-rpcport=$RPCPORT" --bitcoin-rpcuser=soak \
    --bitcoin-rpcpassword=soak "--bitcoin-cli=$W/bin/bitcoin-cli" \
    "--addr=127.0.0.1:$CHRIS_P2P" --alias=chris --grpc-port=9751 \
    "--plugin=$HOLD" --hold-grpc-port=-1 --force-feerates=1000
}
mapfile -t CLN_CMD < <(cln_cmd)
"${CLN_CMD[@]}" > "$W/chris/start.out" 2>&1 || { cat "$W/chris/start.out"; fail "chris did not start"; }
clc() { lightning-cli "--network=$NET" --lightning-dir="$W/chris" "$@"; }
wait_until 60 clc getinfo || { tail -40 "$W/chris/log"; fail "chris does not answer"; }

log "LDK Server: erin"
ERIN_P2P=9739; ERIN_GRPC=3537
mkdir -p "$W/erin"
cat > "$W/erin/config.toml" <<CONF
[node]
network = "$NET"
listening_addresses = ["127.0.0.1:$ERIN_P2P"]
announcement_addresses = ["127.0.0.1:$ERIN_P2P"]
grpc_service_address = "127.0.0.1:$ERIN_GRPC"
alias = "erin"

[storage.disk]
dir_path = "$W/erin/data"

[log]
level = "Debug"
log_to_file = true

[bitcoind]
rpc_address = "127.0.0.1:$RPCPORT"
rpc_user = "soak"
rpc_password = "soak"
CONF
"$W/target-ldk/debug/ldk-server" "$W/erin/config.toml" > "$W/erin/server.log" 2>&1 &
ERIN_CERT=$W/erin/data/tls.crt; ERIN_MAC=$W/erin/data/$NET/macaroons/admin.macaroon
ldkw() { LDK_ADDR=127.0.0.1:$ERIN_GRPC LDK_CERT=$ERIN_CERT LDK_MAC=$ERIN_MAC "$W/target/release/ldk-wallet" "$@"; }
erin_up() { test -s "$ERIN_CERT" && test -s "$ERIN_MAC" && ldkw id; }
wait_until 180 erin_up || { tail -40 "$W/erin/server.log" | cut -c1-300; fail "erin did not come up on $NET"; }

declare -A ID
for n in alice bob carol; do ID[$n]=$(lnc "$n" getinfo | jget identity_pubkey); done
ID[chris]=$(clc getinfo | jget id); ID[erin]=$(ldkw id)

log "fund every node"
# Every node opens channels again and again: each open needs a confirmed output of its own (LND,
# for one, spends no unconfirmed change by default), so each gets several.
for _ in 1 2 3 4 5 6; do
  for n in alice bob; do $BCLI -rpcwallet=miner sendtoaddress "$(lnc "$n" newaddress p2wkh | jget address)" 1 >/dev/null; done
  $BCLI -rpcwallet=miner sendtoaddress "$(clc newaddr p2tr | jget p2tr)" 1 >/dev/null
  $BCLI -rpcwallet=miner sendtoaddress "$(ldkw address)" 1 >/dev/null
  mine 1  # the miner's own change, confirmed: no long chain of unconfirmed sends
done
for _ in 1 2 3 4 5; do $BCLI -rpcwallet=miner sendtoaddress "$(lnc carol newaddress p2wkh | jget address)" 3 >/dev/null; done
mine 6
lnd_funded() { [ "$(lnc "$1" walletbalance | jget confirmed_balance)" -gt 0 ]; }
chris_funded() { [ "$(clc listfunds | python3 -c 'import json,sys; print(sum(1 for o in json.load(sys.stdin)["outputs"] if o["status"] == "confirmed"))')" -gt 0 ]; }
erin_funded() { [ "$(ldkw spendable)" -gt 0 ]; }
for n in alice bob carol; do wait_until 120 lnd_funded "$n" || fail "$n's wallet did not see its funds"; done
wait_until 180 chris_funded || fail "chris's wallet did not see its funds"
wait_until 240 erin_funded || fail "erin's wallet did not see its funds"

log "the payment mesh: every agent's node and carol open 2000000 sat channels to each other"
for n in alice bob; do lnc "$n" connect "${ID[carol]}@127.0.0.1:${P2P[carol]}" >/dev/null 2>&1 || true; done
clc connect "${ID[carol]}" 127.0.0.1 "${P2P[carol]}" >/dev/null 2>&1 || true
lnc carol connect "${ID[chris]}@127.0.0.1:$CHRIS_P2P" >/dev/null 2>&1 || true
lnc carol connect "${ID[erin]}@127.0.0.1:$ERIN_P2P" >/dev/null 2>&1 || true
for n in alice bob; do
  wait_until 60 lnc "$n" openchannel --node_key "${ID[carol]}" --local_amt 2000000 --sat_per_vbyte 2 \
    || fail "$n could not open to carol"
done
wait_until 60 clc fundchannel "${ID[carol]}" 2000000 2000perkw true || fail "chris could not open to carol"
wait_until 60 ldkw open "${ID[carol]}" "127.0.0.1:${P2P[carol]}" 2000000 || fail "erin could not open to carol"
mine 1
for n in alice bob; do
  wait_until 60 lnc carol openchannel --node_key "${ID[$n]}" --local_amt 2000000 --sat_per_vbyte 2 \
    || fail "carol could not open to $n"
done
wait_until 60 lnc carol openchannel --node_key "${ID[chris]}" --local_amt 2000000 --sat_per_vbyte 2 \
  || fail "carol could not open to chris"
wait_until 60 lnc carol openchannel --node_key "${ID[erin]}" --local_amt 2000000 --sat_per_vbyte 2 \
  || fail "carol could not open to erin"
mine 8
carol_chans() { [ "$(lnc carol listchannels --active_only | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["channels"]))')" -ge 8 ]; }
wait_until 300 carol_chans || { lnc carol listchannels | head -40; fail "carol does not have all eight channels active"; }
# Payments route through carol: every node must know every channel. Announcements need six blocks.
mine 6
graph_full() { [ "$(lnc "$1" describegraph | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["edges"]))')" -ge 8 ]; }
for n in alice bob; do wait_until 300 graph_full "$n" || fail "$n's graph did not fill"; done

log "the driver takes over: relays, the instance, the agents as daemons"
read -r INSTANCE_NSEC INSTANCE_PUB < <("$W/target/release/test-key")
read -r SOLVER_NSEC SOLVER_PUB < <("$W/target/release/test-key")
mkdir -p "$W/instance"
# The instance's own copy of carol's macaroon, owner-only: bipnipd refuses one others can read.
install -m 600 "$(mac carol)" "$W/instance/admin.macaroon"
python3 - "$W/bipnipd/settings.tpl.toml" "$W/instance/settings.toml" <<PY
import re, sys
text = open(sys.argv[1]).read()
values = {
    "lnd_cert_file": "'$W/carol/tls.cert'",
    "lnd_macaroon_file": "'$W/instance/admin.macaroon'",
    "lnd_grpc_host": "'https://127.0.0.1:${RPC[carol]}'",
    "nsec_privkey": "'$INSTANCE_NSEC'",
    "relays": "['ws://127.0.0.1:7777', 'ws://127.0.0.1:7778']",
    "bitcoin_price_api_url": '"http://127.0.0.1:9"',
    "url": '"sqlite://$W/instance/mostro.db"',
    "pow": "0",
    "fee": "0.01",
}
for key, value in values.items():
    text, n = re.subn(rf"(?m)^{key} = .*$", f"{key} = {value}", text)
    assert n == 1, (key, n)
text += "\n[liquidity]\nbonds = true\nmax_bond_sat = 100000\nlease_bond_window_secs = 1800\n"
open(sys.argv[2], "w").write(text)
PY
chmod 600 "$W/instance/settings.toml"

agent_conf() { # name, node section
  cat > "$W/$1.toml" <<CONF
network = "$NET"
data_dir = "$W/$1-agent"
relays = ["ws://127.0.0.1:7777", "ws://127.0.0.1:7778"]

$2

[[instance]]
pubkey = "$INSTANCE_PUB"

[policy]
max_runs = 8
max_fee_per_lease_sat = 50000
max_fee_per_day_sat = 1000000
max_capacity_per_day_sat = 50000000
max_bond_sat = 100000
max_bond_per_day_sat = 1000000
max_feerate_sat_vb = 5
min_sale_ppb_per_block = 0
CONF
  "$AGENT" --config "$W/$1.toml" init > /dev/null
}
for n in alice bob; do
  agent_conf "$n" "[lnd]
address = \"https://127.0.0.1:${RPC[$n]}\"
tls_cert = \"$W/$n/tls.cert\"
macaroon = \"$(mac "$n")\""
done
agent_conf chris "[cln]
rpc_socket = \"$W/chris/$NET/lightning-rpc\""
agent_conf erin "[ldk]
address = \"127.0.0.1:$ERIN_GRPC\"
tls_cert = \"$ERIN_CERT\"
macaroon = \"$ERIN_MAC\""

# What the driver runs and restarts, as argv lists, and how to tell each is up.
python3 - "$W/soak/procs.json" <<PY
import json, sys
W = "$W"
procs = {
    "relay1": {"argv": [f"{W}/target/release/test-relay", "7777"], "ready": {"port": 7777}},
    "relay2": {"argv": [f"{W}/target/release/test-relay", "7778"], "ready": {"port": 7778}},
    "instance": {"argv": [f"{W}/target-d/release/mostrod", "--dirsettings", f"{W}/instance"],
                 "env": {"TERM": "xterm"}, "ready": {"log": "Scheduler Started"}},
}
for n in ["alice", "bob", "chris", "erin"]:
    procs[n] = {"argv": ["$AGENT", "--config", f"{W}/{n}.toml", "run", "--poll", "2"],
                "ready": {"socket": f"{W}/{n}-agent/agent.sock"}}
json.dump(procs, open(sys.argv[1], "w"), indent=1)
PY
printf '%s\n' "$(lnd_cmd carol)" > "$W/soak/carol.argv"

export W NET BCLI MADDR SOAK_HOURS SOAK_BLOCK_SECS SOAK_AUDIT_SECS SOAK_FAULT_SECS SOAK_SEED
export INSTANCE_PUB INSTANCE_NSEC SOLVER_NSEC SOLVER_PUB
export MCLI="$W/target-m/release/mostro-cli" INSTANCE_DB="$W/instance/mostro.db"
LNC_SELLER="lncli --lnddir=$W/alice --network=$NET --rpcserver=127.0.0.1:${RPC[alice]}"
LNC_BUYER="lncli --lnddir=$W/bob --network=$NET --rpcserver=127.0.0.1:${RPC[bob]}"
LNC_CAROL="lncli --lnddir=$W/carol --network=$NET --rpcserver=127.0.0.1:${RPC[carol]}"
export LNC_SELLER LNC_BUYER LNC_CAROL
export MOSTRO_PUBKEY="$INSTANCE_PUB" RELAYS="ws://127.0.0.1:7777,ws://127.0.0.1:7778" POW=0
export SOAK_NODES="alice=${ID[alice]},bob=${ID[bob]},carol=${ID[carol]},chris=${ID[chris]},erin=${ID[erin]}"
export SOAK_ADDRS="alice=127.0.0.1:${P2P[alice]},bob=127.0.0.1:${P2P[bob]},chris=127.0.0.1:$CHRIS_P2P,erin=127.0.0.1:$ERIN_P2P"
export SOAK_CLN_DIR="$W/chris" FIAT_PY="$LN/fiat.py"
python3 -u "$HERE/soak.py"
