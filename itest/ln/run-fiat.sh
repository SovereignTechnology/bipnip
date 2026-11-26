#!/usr/bin/env bash
# Fiat trades through a real bipnipd, the way Mostro's own users trade, in a throwaway Kata VM with
# network, as root:
#
#   kata-run --cwd ~/Projects/bipnip --net --timeout 5400 -- bash itest/ln/run-fiat.sh
#
# Upstream's command-line client, mostro-cli (pinned), drives both sides, so this is also the check
# that an upstream Mostro client still trades on a bipnip instance. Regtest: bitcoind and three LND
# nodes, the seller (alice), the buyer (bob) and the instance's node (carol), with channels
# alice -> carol -> bob. `itest/ln/fiat.py` answers each side as the instance asks:
#
#   1. a sell order: made by the seller, taken by the buyer, released, paid out;
#   2. a buy order: made by the buyer, taken by the seller, released, paid out;
#   3. a cooperative cancel once the sats are held: the hold invoice is canceled, the buyer gets nothing;
#   4. a dispute: the seller does not release, the buyer opens a dispute, and a solver (added with the
#      instance's key) takes it and settles it for the buyer;
#   5. a gap: the instance is stopped across the seller's release, which is lost; the seller sends it
#      again once the instance answers, and the trade pays out (#63).
#
# FIAT_STEPS picks trades (default: all five, in order), e.g. FIAT_STEPS=gap.
#
# Every key and credential here is a throwaway regtest value inside the VM.

set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
W=${ITEST_WORK:-/tmp/bipnip-fiat-itest}
rm -rf "$W"; mkdir -p "$W"/{dl,bin,btc,instance}
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
# The instance: bipnipd with Mostro v0.20.3 merged in.
BIPNIPD_REV=${BIPNIPD_REV:-342990f254a40d350084147a4eed481366408d49}
# mostro-cli v0.17.1, upstream's command-line client (mostro-core 0.17.3 on the wire).
MCLI_REV=0665895f0a9806756252bd34a0572416f7c13e61

log "tools"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq protobuf-compiler python3 curl ca-certificates git cmake build-essential \
  pkg-config libssl-dev >/dev/null

log "downloads (sha256-pinned)"
fetch "https://bitcoincore.org/bin/bitcoin-core-$BTC_V/bitcoin-$BTC_V-x86_64-linux-gnu.tar.gz" "$BTC_SHA" "$W/dl/btc.tgz"
fetch "https://github.com/lightningnetwork/lnd/releases/download/$LND_V/lnd-linux-amd64-$LND_V.tar.gz" "$LND_SHA" "$W/dl/lnd.tgz"
tar xzf "$W/dl/btc.tgz" -C "$W/dl"; cp "$W/dl/bitcoin-$BTC_V/bin/"{bitcoind,bitcoin-cli} "$W/bin/"
tar xzf "$W/dl/lnd.tgz" -C "$W/dl"; cp "$W/dl/lnd-linux-amd64-$LND_V/"{lnd,lncli} "$W/bin/"
export PATH=$W/bin:$PATH

log "builds: the instance (bipnipd ${BIPNIPD_REV:0:12}), mostro-cli (${MCLI_REV:0:12}), the test helpers"
git clone --quiet https://github.com/SovereignTechnology/bipnipd "$W/bipnipd"
git -C "$W/bipnipd" checkout --quiet "$BIPNIPD_REV"
git clone --quiet https://github.com/MostroP2P/mostro-cli "$W/mostro-cli"
git -C "$W/mostro-cli" checkout --quiet "$MCLI_REV"
[ "$(git -C "$W/mostro-cli" rev-parse HEAD)" = "$MCLI_REV" ] || fail "mostro-cli is not at $MCLI_REV"
# Each tree may pin its own toolchain; let rustup install them one at a time (two installs of the
# same toolchain at once collide in its download directory).
for tree in "$W/bipnipd" "$W/mostro-cli" "$HERE"; do
  (cd "$tree" && cargo --version) > /dev/null || fail "no Rust toolchain for $tree"
done
cargo build --quiet --release --locked --manifest-path "$W/bipnipd/Cargo.toml" --bin mostrod \
  --target-dir "$C/target-d" > "$W/build-d.log" 2>&1 &
BUILD_D=$!
(cd "$W/mostro-cli" && cargo build --quiet --release --locked --target-dir "$C/target-m") \
  > "$W/build-m.log" 2>&1 &
BUILD_M=$!
cargo build --quiet --release --manifest-path "$HERE/Cargo.toml" --target-dir "$C/target" \
  --bin test-relay --bin test-key
wait "$BUILD_D" || { tail -30 "$W/build-d.log"; fail "the instance did not build"; }
wait "$BUILD_M" || { tail -30 "$W/build-m.log"; fail "mostro-cli did not build"; }

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

log "three LND nodes: alice sells, bob buys, carol is the instance's node"
declare -A RPC=([alice]=10009 [bob]=10010 [carol]=10011) REST=([alice]=8081 [bob]=8082 [carol]=8083) \
  P2P=([alice]=9735 [bob]=9736 [carol]=9737)
for n in alice bob carol; do
  mkdir -p "$W/$n"
  lnd --lnddir="$W/$n" --bitcoin.regtest --bitcoin.node=bitcoind \
    --bitcoind.rpchost=127.0.0.1:18443 --bitcoind.rpcuser=itest --bitcoind.rpcpass=itest \
    --bitcoind.zmqpubrawblock=tcp://127.0.0.1:28332 --bitcoind.zmqpubrawtx=tcp://127.0.0.1:28333 \
    --noseedbackup --rpclisten=127.0.0.1:${RPC[$n]} --restlisten=127.0.0.1:${REST[$n]} \
    --listen=127.0.0.1:${P2P[$n]} --alias="$n" --trickledelay=1000 > "$W/$n/lnd.log" 2>&1 &
done
lncmd() { echo "lncli --lnddir=$W/$1 --network=regtest --rpcserver=127.0.0.1:${RPC[$1]}"; }
lnc() { local n=$1; shift; $(lncmd "$n") "$@"; }
synced() { [ "$(lnc "$1" getinfo | jget synced_to_chain)" = "True" ]; }
for n in alice bob carol; do wait_until 120 synced "$n" || { tail -20 "$W/$n/lnd.log"; fail "$n did not sync"; }; done
mac() { echo "$W/$1/data/chain/bitcoin/regtest/admin.macaroon"; }

log "fund alice and carol; channels alice -> carol -> bob"
for n in alice carol; do $BCLI -rpcwallet=miner sendtoaddress "$(lnc "$n" newaddress p2wkh | jget address)" 5 >/dev/null; done
$BCLI generatetoaddress 6 "$MADDR" >/dev/null
funded() { [ "$(lnc "$1" walletbalance | jget confirmed_balance)" -gt 0 ]; }
for n in alice carol; do wait_until 60 funded "$n" || fail "$n was not funded"; done
lnc alice connect "$(lnc carol getinfo | jget identity_pubkey)@127.0.0.1:${P2P[carol]}" >/dev/null
lnc carol connect "$(lnc bob getinfo | jget identity_pubkey)@127.0.0.1:${P2P[bob]}" >/dev/null
lnc alice openchannel --node_key "$(lnc carol getinfo | jget identity_pubkey)" --local_amt 1000000 >/dev/null
lnc carol openchannel --node_key "$(lnc bob getinfo | jget identity_pubkey)" --local_amt 1000000 >/dev/null
$BCLI generatetoaddress 6 "$MADDR" >/dev/null
active() { [ "$(lnc "$1" listchannels --active_only | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["channels"]))')" -ge "$2" ]; }
wait_until 120 active carol 2 || fail "the channels did not become active"

log "the relay"
"$C/target/release/test-relay" 7777 > "$W/relay.log" 2>&1 &
wait_until 30 bash -c 'exec 3<>/dev/tcp/127.0.0.1/7777' || fail "the relay did not start"

log "instance: settings and start"
read -r INSTANCE_NSEC INSTANCE_PUB < <("$C/target/release/test-key")
read -r SOLVER_NSEC SOLVER_PUB < <("$C/target/release/test-key")
# The daemon's own template, with this run's node, relay, key and database. Orders here name their
# amount in sats, so no price is looked up: the price source is a closed local port. A 1 % fee, so
# the amounts the instance asks for are checked with a fee in them.
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
    "fee": "0.01",
}
for key, value in values.items():
    text, n = re.subn(rf"(?m)^{key} = .*$", f"{key} = {value}", text)
    assert n == 1, (key, n)
open(sys.argv[2], "w").write(text)
PY
chmod 600 "$W/instance/settings.toml"
TERM=xterm "$C/target-d/release/mostrod" --dirsettings "$W/instance" > "$W/instance/mostrod.log" 2>&1 < /dev/null &
INSTANCE_PID=$!
instance_log() { sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$W/instance/mostrod.log" | tail -"${1:-40}" | sed 's/^/  instance: /'; }
wait_until 120 grep -q "Scheduler Started" "$W/instance/mostrod.log" || { instance_log; fail "the instance did not start"; }

export W INSTANCE_NSEC SOLVER_NSEC SOLVER_PUB INSTANCE_PID
export MCLI="$C/target-m/release/mostro-cli" INSTANCE_DB="$W/instance/mostro.db"
LNC_SELLER=$(lncmd alice)
LNC_BUYER=$(lncmd bob)
export LNC_SELLER LNC_BUYER
export MOSTRO_PUBKEY="$INSTANCE_PUB" RELAYS="ws://127.0.0.1:7777" POW=0

log "mostro-cli: identities for the seller, the buyer and a solver"
python3 "$HERE/fiat.py" setup || { instance_log 60; fail "setup"; }
for step in ${FIAT_STEPS:-sell buy cancel dispute gap}; do
  case "$step" in sell|buy|cancel|dispute|gap) ;; *) fail "FIAT_STEPS: no trade called $step" ;; esac
  log "trade: $step"
  python3 "$HERE/fiat.py" "$step" || { instance_log 80; fail "the $step trade"; }
done

[ -z "${FIAT_STEPS:-}" ] || { log "FIAT E2E PASS, these trades only: $FIAT_STEPS"; exit 0; }
log "FIAT E2E PASS: upstream mostro-cli traded on a bipnip instance with real LND nodes: a sell and a buy paid out, a cooperative cancel returned the held sats, a dispute was settled by a solver, and a release lost while the instance was stopped was sent again and paid out"
