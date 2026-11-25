#!/usr/bin/env bash
# Agents on three kinds of node trade through one real instance, in a throwaway Kata VM with
# network, as root:
#
#   kata-run --cwd ~/Projects/bipnip --net --timeout 3400 -- bash itest/ln/run-mixed.sh
#
# Regtest: bitcoind; an LND node (alice), a Core Lightning node with the hold plugin (chris) and an
# LDK Server (erin), each with a bipnip agent; a second LND node (carol) for a real bipnipd.
#
#   1. a lease sold by the LND agent to the Core Lightning agent;
#   2. a lease sold by the Core Lightning agent to the LND agent;
#   3. a ring of three: LND, Core Lightning and LDK each open one channel to the next;
#   4. a lease sold by the LND agent to the LDK agent.
#
# An LDK node sells no lease (bipnip-ln's `ldk` module says why); that its agent refuses to offer
# one is checked here too.
# Every key and credential here is a throwaway regtest value inside the VM.

set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
W=${ITEST_WORK:-/tmp/bipnip-mixed-itest}
rm -rf "$W"; mkdir -p "$W"/{dl,bin,btc,cln}
. "$HERE/cache.sh"   # $C: downloads and target dirs kept between runs (cache.sh)
log() { printf '[itest %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
fail() { log "FAIL: $*"; exit 1; }
jget() { python3 -c 'import json,sys
d=json.load(sys.stdin)
for k in sys.argv[1:]: d=d[k]
print(d)' "$@"; }
wait_until() { local secs=$1; shift; local t=0; until "$@" >/dev/null 2>&1; do sleep 1; t=$((t+1)); [ "$t" -lt "$secs" ] || return 1; done; }

BTC_V=30.4
BTC_SHA=051a18c595d9a27df23cb7187ff9fb343e90943cb142af751e12787806500c3d
LND_V=v0.21.5-beta
LND_SHA=2127d4794ea9cbd98e762ba16283615158743646d2d6d2a5bf88664dd6255ce7
CLN_V=v26.06.10
CLN_SHA=e1e0a7e89b55060e2470d8568ba535a3124d6823c628def548cadeef4279c491
HOLD_V=v0.3.3
HOLD_SHA=f9d916430fb70a3d75089f2d1b88d5d71d4987ef79933035978c1f44aab84be5
LDK_SERVER_REV=d7e7a2e708664af922c53c786ad6a5df622f5dd8
# The instance: the same revision run-lnd.sh uses.
BIPNIPD_REV=028099e547a30a929fe0dcee36ef4d15d19a3b43

log "tools"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq protobuf-compiler python3 curl xz-utils ca-certificates git cmake clang pkg-config libpq5 libsqlite3-0 libsodium23 zlib1g >/dev/null

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

log "build: the test programs, the agent for all three kinds of node, LDK Server, the instance"
cargo build --quiet --locked --release --manifest-path "$HERE/Cargo.toml" --target-dir "$C/target"
cargo build --quiet --locked --release --manifest-path "$HERE/../../Cargo.toml" -p bipnip-agent --features lnd,cln,ldk --target-dir "$C/target-ws"
AGENT=$C/target-ws/release/bipnip-agent
git clone -q https://github.com/lightningdevkit/ldk-server "$W/ldk-server"
git -C "$W/ldk-server" checkout -q "$LDK_SERVER_REV"
( cd "$W/ldk-server" && cargo build --quiet --locked -p ldk-server --target-dir "$C/target-ldk" )
LDK_SERVER=$C/target-ldk/debug/ldk-server
git clone --quiet https://github.com/SovereignTechnology/bipnipd "$W/bipnipd"
git -C "$W/bipnipd" checkout --quiet "$BIPNIPD_REV"
cargo build --quiet --release --locked --manifest-path "$W/bipnipd/Cargo.toml" --bin mostrod --target-dir "$C/target-d"

log "bitcoind regtest"
cat > "$W/btc/bitcoin.conf" <<CONF
regtest=1
server=1
fallbackfee=0.0002
[regtest]
rpcuser=itest
rpcpassword=itest
rpcport=18443
rpcbind=127.0.0.1
rpcallowip=127.0.0.1
zmqpubrawblock=tcp://127.0.0.1:28332
zmqpubrawtx=tcp://127.0.0.1:28333
CONF
bitcoind -datadir="$W/btc" -daemon
BCLI="bitcoin-cli -datadir=$W/btc"
wait_until 60 $BCLI getblockchaininfo
$BCLI createwallet miner >/dev/null
MADDR=$($BCLI -rpcwallet=miner getnewaddress)
$BCLI generatetoaddress 101 "$MADDR" >/dev/null
printf '#!/usr/bin/env bash\nexec %s generatetoaddress "$1" %s >/dev/null\n' "$BCLI" "$MADDR" > "$W/mine.sh"; chmod +x "$W/mine.sh"

log "LND: alice (an agent's node) and carol (the instance's node)"
declare -A RPC=([alice]=10009 [carol]=10011) REST=([alice]=8081 [carol]=8083) P2P=([alice]=9735 [carol]=9737)
for n in alice carol; do
  mkdir -p "$W/$n"
  lnd --lnddir="$W/$n" --bitcoin.regtest --bitcoin.node=bitcoind \
    --bitcoind.rpchost=127.0.0.1:18443 --bitcoind.rpcuser=itest --bitcoind.rpcpass=itest \
    --bitcoind.zmqpubrawblock=tcp://127.0.0.1:28332 --bitcoind.zmqpubrawtx=tcp://127.0.0.1:28333 \
    --noseedbackup --rpclisten=127.0.0.1:${RPC[$n]} --restlisten=127.0.0.1:${REST[$n]} \
    --listen=127.0.0.1:${P2P[$n]} --alias="$n" --trickledelay=1000 > "$W/$n/lnd.log" 2>&1 &
done
lnc() { local n=$1; shift; lncli --lnddir="$W/$n" --network=regtest --rpcserver=127.0.0.1:${RPC[$n]} "$@"; }
synced() { [ "$(lnc "$1" getinfo | jget synced_to_chain)" = "True" ]; }
for n in alice carol; do wait_until 120 synced "$n" || { tail -20 "$W/$n/lnd.log"; fail "$n did not sync"; }; done
mac() { echo "$W/$1/data/chain/bitcoin/regtest/admin.macaroon"; }

log "Core Lightning with the hold plugin: chris"
CHRIS_P2P=9741
mkdir -p "$W/chris"
start_chris() {
  lightningd --network=regtest --lightning-dir="$W/chris" --daemon --log-file="$W/chris/log" \
    --bitcoin-rpcconnect=127.0.0.1 --bitcoin-rpcport=18443 --bitcoin-rpcuser=itest \
    --bitcoin-rpcpassword=itest --bitcoin-cli="$W/bin/bitcoin-cli" \
    --addr=127.0.0.1:$CHRIS_P2P --alias=chris --grpc-port=9751 \
    --plugin="$HOLD" --hold-grpc-port=-1 "$@"
}
clc() { lightning-cli --network=regtest --lightning-dir="$W/chris" "$@"; }
start_chris --developer --dev-fast-gossip --dev-bitcoind-poll=1 > "$W/chris/start.out" 2>&1 \
  || start_chris > "$W/chris/start.out" 2>&1 \
  || { cat "$W/chris/start.out"; fail "chris did not start"; }
wait_until 60 clc getinfo || { tail -40 "$W/chris/log"; fail "chris does not answer"; }

log "LDK Server: erin"
ERIN_P2P=9739; ERIN_GRPC=3537
mkdir -p "$W/erin"
cat > "$W/erin/config.toml" <<CONF
[node]
network = "regtest"
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
rpc_address = "127.0.0.1:18443"
rpc_user = "itest"
rpc_password = "itest"
CONF
"$LDK_SERVER" "$W/erin/config.toml" > "$W/erin/server.log" 2>&1 &
ERIN_CERT=$W/erin/data/tls.crt; ERIN_MAC=$W/erin/data/regtest/macaroons/admin.macaroon
ldkw() { LDK_ADDR=127.0.0.1:$ERIN_GRPC LDK_CERT=$ERIN_CERT LDK_MAC=$ERIN_MAC "$C/target/release/ldk-wallet" "$@"; }
erin_up() { test -s "$ERIN_CERT" && test -s "$ERIN_MAC" && ldkw id; }
wait_until 120 erin_up || { tail -40 "$W/erin/server.log" | cut -c1-300; fail "erin did not come up"; }

ALICE=$(lnc alice getinfo | jget identity_pubkey); CHRIS=$(clc getinfo | jget id); ERIN=$(ldkw id)
log "fund the three agents' nodes"
$BCLI -rpcwallet=miner sendtoaddress "$(lnc alice newaddress p2wkh | jget address)" 5 >/dev/null
$BCLI -rpcwallet=miner sendtoaddress "$(clc newaddr p2tr | jget p2tr)" 5 >/dev/null
$BCLI -rpcwallet=miner sendtoaddress "$(ldkw address)" 5 >/dev/null
"$W/mine.sh" 6
alice_funded() { [ "$(lnc alice walletbalance | jget confirmed_balance)" -gt 0 ]; }
chris_funded() { [ "$(clc listfunds | python3 -c 'import json,sys; print(sum(1 for o in json.load(sys.stdin)["outputs"] if o["status"] == "confirmed"))')" -gt 0 ]; }
erin_funded() { [ "$(ldkw spendable)" -gt 0 ]; }
wait_until 60 alice_funded; wait_until 120 chris_funded || fail "chris's wallet did not see its funds"
wait_until 180 erin_funded || fail "erin's wallet did not see its funds"

# A buyer pays its seller over Lightning, so each pair that leases needs a way to pay first.
log "channels to pay over: alice <-> chris (one each way)"
lnc alice connect "$CHRIS@127.0.0.1:$CHRIS_P2P" >/dev/null 2>&1 || true
wait_until 30 lnc alice openchannel --node_key "$CHRIS" --local_amt 1000000 || fail "alice could not open a channel to chris"
"$W/mine.sh" 3
wait_until 60 clc fundchannel "$ALICE" 1000000 normal true || fail "chris could not open a channel to alice"
"$W/mine.sh" 6
alice_chans() { [ "$(lnc alice listchannels --active_only | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["channels"]))')" -ge "$1" ]; }
chris_chans() { [ "$(clc listpeerchannels | python3 -c 'import json,sys;print(sum(1 for c in json.load(sys.stdin)["channels"] if c["state"] == "CHANNELD_NORMAL"))')" -ge "$1" ]; }
wait_until 180 alice_chans 2 || fail "alice does not have both channels with chris active"
wait_until 180 chris_chans 2 || fail "chris does not have both channels with alice normal"

"$C/target/release/test-relay" 7777 > "$W/relay.log" 2>&1 &
wait_until 30 grep -q listening "$W/relay.log"

log "instance: settings and start"
read -r INSTANCE_NSEC INSTANCE_PUB < <("$C/target/release/test-key")
mkdir -p "$W/instance"
# The daemon's own template, with this run's node, relay, key and database. The price source is
# pointed at a closed local port: this run makes no outside calls and trades no fiat.
python3 - "$W/bipnipd/settings.tpl.toml" "$W/instance/settings.toml" <<PY
import re, sys
text = open(sys.argv[1]).read()
values = {
    "lnd_cert_file": "'$W/carol/tls.cert'",
    "lnd_macaroon_file": "'$(mac carol)'",
    "lnd_grpc_host": "'https://127.0.0.1:${RPC[carol]}'",
    "nsec_privkey": "'$INSTANCE_NSEC'",
    "relays": "['ws://127.0.0.1:7777']",
    "bitcoin_price_api_url": '"http://127.0.0.1:9"',
    "url": '"sqlite://$W/instance/mostro.db"',
    "pow": "0",
}
for key, value in values.items():
    text, n = re.subn(rf"(?m)^{key} = .*$", f"{key} = {value}", text)
    assert n == 1, (key, n)
# This instance takes ring bonds: its node (carol) holds them.
# A lease buyer's bond is decided 180 seconds after the take (48 hours by default).
# Its node starts with no peer, so its graph knows no seller yet (run-lnd.sh says more).
text += "\n[liquidity]\nbonds = true\nmax_bond_sat = 100000\nlease_bond_window_secs = 180\nrequire_seller_in_graph = false\n"
open(sys.argv[2], "w").write(text)
PY
chmod 600 "$W/instance/settings.toml"
TERM=xterm "$C/target-d/release/mostrod" --dirsettings "$W/instance" > "$W/instance/mostrod.log" 2>&1 < /dev/null &
INSTANCE_PID=$!
instance_log() { sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$W/instance/mostrod.log" | tail -"${1:-40}" | sed 's/^/  instance: /'; }
wait_until 120 grep -q "Scheduler Started" "$W/instance/mostrod.log" \
  || { instance_log; fail "the instance did not start"; }

log "the three agents"
agent_conf() { # name, node section
  cat > "$W/$1.toml" <<CONF
network = "regtest"
data_dir = "$W/$1-agent"
relays = ["ws://127.0.0.1:7777"]

$2

[instance]
pubkey = "$INSTANCE_PUB"
pow = 0
CONF
  "$AGENT" --config "$W/$1.toml" init > /dev/null
}
agent_conf alice "[lnd]
address = \"https://127.0.0.1:${RPC[alice]}\"
tls_cert = \"$W/alice/tls.cert\"
macaroon = \"$(mac alice)\""
agent_conf chris "[cln]
rpc_socket = \"$W/chris/regtest/lightning-rpc\""
agent_conf erin "[ldk]
address = \"127.0.0.1:$ERIN_GRPC\"
tls_cert = \"$ERIN_CERT\"
macaroon = \"$ERIN_MAC\""
for n in alice chris erin; do
  "$AGENT" --config "$W/$n.toml" whoami > "$W/$n-whoami.out" || { cat "$W/$n-whoami.out"; fail "$n's agent cannot reach its node"; }
done
grep -q "$ALICE" "$W/alice-whoami.out" && grep -q "$CHRIS" "$W/chris-whoami.out" && grep -q "$ERIN" "$W/erin-whoami.out" \
  || fail "an agent does not show its own node"
dump() {
  for f in "$W"/mx-*.out "$W"/mx-*.err; do [ -s "$f" ] && sed "s|^|  $(basename "$f"): |" "$f" | tail -30; done
  instance_log 60
}
# One lease from offer to active: seller, buyer, capacity, the buyer's address, a name for files.
lease() {
  local seller=$1 buyer=$2 capacity=$3 address=$4 tag=$5
  "$AGENT" --config "$W/$seller.toml" lease offer --min-capacity 300000 --max-capacity 800000 \
    --duration 144 --fee-base 1000 --fee-ppm 2000 --feerate 2 --confs 3 --expires-in 3600 --poll 2 \
    > "$W/mx-$tag-offer.out" 2> "$W/mx-$tag-offer.err" &
  local offer_pid=$!
  wait_until 90 grep -q "state=open" "$W/mx-$tag-offer.out" || { dump; fail "$tag: the offer did not open"; }
  local order; order=$(grep -m1 "state=open" "$W/mx-$tag-offer.out" | sed 's/.*order=\([^ ]*\).*/\1/')
  "$AGENT" --config "$W/$buyer.toml" lease take "$order" --capacity "$capacity" \
    --node-address "$address" --poll 2 > "$W/mx-$tag-take.out" 2> "$W/mx-$tag-take.err" &
  local take_pid=$!
  local t=0
  until grep -q "state=active" "$W/mx-$tag-offer.out" && grep -q "state=active" "$W/mx-$tag-take.out"; do
    kill -0 "$offer_pid" 2>/dev/null || grep -q "state=active" "$W/mx-$tag-offer.out" || { dump; fail "$tag: the seller's agent stopped early"; }
    kill -0 "$take_pid" 2>/dev/null || grep -q "state=active" "$W/mx-$tag-take.out" || { dump; fail "$tag: the buyer's agent stopped early"; }
    "$W/mine.sh" 1; sleep 2; t=$((t+1))
    [ "$t" -lt 120 ] || { dump; fail "$tag: the lease did not become active"; }
  done
  wait "$offer_pid" || fail "$tag: the seller's agent exited with an error"
  wait "$take_pid" || fail "$tag: the buyer's agent exited with an error"
  tail -1 "$W/mx-$tag-offer.out" | sed 's/^/  seller: /'; tail -1 "$W/mx-$tag-take.out" | sed 's/^/  buyer:  /'
}
cln_has() { # capacity in sat, opener: chris has exactly one usable channel of that size, opened by that side
  clc listpeerchannels | python3 -c '
import json, sys
found = [c for c in json.load(sys.stdin)["channels"] if c["total_msat"] == int(sys.argv[1]) * 1000]
assert len(found) == 1 and found[0]["opener"] == sys.argv[2] and found[0]["state"] == "CHANNELD_NORMAL", found
' "$1" "$2"
}
lnd_has() { # capacity in sat, initiator (True/False): alice has exactly one active channel of that size
  lnc alice listchannels | python3 -c '
import json, sys
found = [c for c in json.load(sys.stdin)["channels"] if c["capacity"] == sys.argv[1]]
assert len(found) == 1 and str(found[0]["initiator"]) == sys.argv[2] and found[0]["active"], found
' "$1" "$2"
}
ldk_has() { # capacity in sat, direction (in/out): erin has exactly one usable channel of that size
  [ "$(ldkw channels | grep -c "^$1 $2 usable$")" = 1 ]
}

log "lease 1: the LND agent (alice) sells 410000 sat to the Core Lightning agent (chris)"
lease alice chris 410000 "127.0.0.1:$CHRIS_P2P" l1
lnd_has 410000 True || fail "alice did not open the 410000 sat channel"
wait_until 60 cln_has 410000 remote || fail "chris does not have the 410000 sat channel it leased"

log "lease 2: the Core Lightning agent (chris) sells 420000 sat to the LND agent (alice)"
lease chris alice 420000 "127.0.0.1:${P2P[alice]}" l2
wait_until 60 cln_has 420000 local || fail "chris did not open the 420000 sat channel"
lnd_has 420000 False || fail "alice does not have the 420000 sat channel it leased"

log "an LDK agent offers no lease"
if "$AGENT" --config "$W/erin.toml" lease offer --min-capacity 300000 --max-capacity 800000 \
  --duration 144 --fee-base 1000 --fee-ppm 2000 --feerate 2 > "$W/mx-erin-offer.out" 2> "$W/mx-erin-offer.err"; then
  fail "the LDK agent offered a lease"
fi
grep -q "cannot sell a lease" "$W/mx-erin-offer.err" || { cat "$W/mx-erin-offer.err"; fail "the LDK agent's offer was refused for another reason"; }

log "ring: alice (LND) proposes a ring of three 250000 sat channels; chris (Core Lightning) and erin (LDK) join"
"$AGENT" --config "$W/alice.toml" ring create --capacity 250000 --slots 3 --feerate 2 \
  --window 3600 --expires-in 3600 --node-address "127.0.0.1:${P2P[alice]}" --poll 2 \
  > "$W/mx-ring-alice.out" 2> "$W/mx-ring-alice.err" &
RING_PIDS=($!)
wait_until 90 grep -q "state=seated" "$W/mx-ring-alice.out" || { dump; fail "the ring was not stored by the instance"; }
RING=$(grep -m1 "state=seated" "$W/mx-ring-alice.out" | sed 's/.*order=\([^ ]*\).*/\1/')
declare -A ADDR=([chris]="127.0.0.1:$CHRIS_P2P" [erin]="127.0.0.1:$ERIN_P2P")
for n in chris erin; do
  "$AGENT" --config "$W/$n.toml" ring join "$RING" --feerate 2 --node-address "${ADDR[$n]}" --poll 2 \
    > "$W/mx-ring-$n.out" 2> "$W/mx-ring-$n.err" &
  RING_PIDS+=($!)
  wait_until 90 grep -q "state=seated\|state=committed\|state=open" "$W/mx-ring-$n.out" || { dump; fail "$n did not get a slot"; }
done
ring_done() { for n in alice chris erin; do grep -q "state=success" "$W/mx-ring-$n.out" || return 1; done; }
t=0
until ring_done; do
  for n in alice chris erin; do
    grep -q "state=partial\|state=canceled" "$W/mx-ring-$n.out" && { dump; fail "the ring ended early for $n"; }
  done
  "$W/mine.sh" 1; sleep 2; t=$((t+1))
  [ "$t" -lt 150 ] || { dump; fail "the ring did not complete"; }
done
for pid in "${RING_PIDS[@]}"; do wait "$pid" || fail "a ring agent exited with an error"; done
for n in alice chris erin; do tail -1 "$W/mx-ring-$n.out" | sed "s/^/  $n: /"; done
lnc alice listchannels | python3 -c '
import json, sys
ring = [c for c in json.load(sys.stdin)["channels"] if c["capacity"] == "250000"]
assert sorted(c["initiator"] for c in ring) == [False, True] and all(c["active"] for c in ring), ring
' || fail "alice does not have one ring channel out and one in"
clc listpeerchannels | python3 -c '
import json, sys
ring = [c for c in json.load(sys.stdin)["channels"] if c["total_msat"] == 250000000]
assert sorted(c["opener"] for c in ring) == ["local", "remote"] and all(c["state"] == "CHANNELD_NORMAL" for c in ring), ring
' || fail "chris does not have one ring channel out and one in"
ldk_has 250000 out && ldk_has 250000 in || { ldkw channels; fail "erin does not have one ring channel out and one in"; }

log "lease 3: the LND agent (alice) sells 430000 sat to the LDK agent (erin)"
# erin pays over the channel it opened in the ring; give the three nodes a moment to hear of it.
"$W/mine.sh" 6; sleep 20
lease alice erin 430000 "127.0.0.1:$ERIN_P2P" l3
lnd_has 430000 True || fail "alice did not open the 430000 sat channel"
wait_until 120 ldk_has 430000 in || { ldkw channels; fail "erin does not have the 430000 sat channel it leased"; }
kill "$INSTANCE_PID"

log "MIXED E2E PASS: agents on LND, Core Lightning and LDK through a real bipnipd: leases LND -> Core Lightning, Core Lightning -> LND and LND -> LDK, and a ring of three across all three kinds of node"
