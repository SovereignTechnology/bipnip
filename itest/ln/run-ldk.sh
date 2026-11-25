#!/usr/bin/env bash
# bipnip-ln conformance for the LDK backend, in a throwaway Kata VM with network, as root:
#
#   kata-run --cwd ~/Projects/bipnip --net --timeout 3300 -- bash itest/ln/run-ldk.sh
#
# Starts regtest bitcoind, two LND nodes from a sha256-pinned release and two LDK Servers built
# from a pinned upstream revision, and runs the same `ln-conformance` steps across the two kinds:
#
#   1. LND pays, LDK is paid (alice -> dave): every payee-side step an LDK node offers. It
#      offers no hold invoices (see bipnip-ln's `ldk` module for why), so those steps say so and
#      are skipped;
#   2. LDK pays, LND is paid (erin -> bob): every payer-side step on LDK, hold invoices included
#      (LND holds, LDK pays);
#   3. a whole lease with an LDK buyer and an LND seller (`lease-e2e`).
#
# An LDK node cannot sell a lease, so that is not run.
# Every key and credential here is a throwaway regtest value inside the VM.

set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
W=${ITEST_WORK:-/tmp/bipnip-ldk-itest}
rm -rf "$W"; mkdir -p "$W"/{dl,bin,btc}
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
# The revision bipnip-ln's `ldk` feature takes its client from.
LDK_SERVER_REV=d7e7a2e708664af922c53c786ad6a5df622f5dd8

log "tools"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq protobuf-compiler python3 curl ca-certificates git cmake clang pkg-config >/dev/null

log "downloads (sha256-pinned)"
fetch "https://bitcoincore.org/bin/bitcoin-core-$BTC_V/bitcoin-$BTC_V-x86_64-linux-gnu.tar.gz" "$BTC_SHA" "$W/dl/btc.tgz"
fetch "https://github.com/lightningnetwork/lnd/releases/download/$LND_V/lnd-linux-amd64-$LND_V.tar.gz" "$LND_SHA" "$W/dl/lnd.tgz"
tar xzf "$W/dl/btc.tgz" -C "$W/dl"; cp "$W/dl/bitcoin-$BTC_V/bin/"{bitcoind,bitcoin-cli} "$W/bin/"
tar xzf "$W/dl/lnd.tgz" -C "$W/dl"; cp "$W/dl/lnd-linux-amd64-$LND_V/"{lnd,lncli} "$W/bin/"
export PATH=$W/bin:$PATH

log "unit tests and clippy of bipnip-ln and the agent with the lnd and ldk features"
cargo clippy --quiet --locked --manifest-path "$HERE/../../Cargo.toml" -p bipnip-ln -p bipnip-agent --features bipnip-ln/lnd,bipnip-ln/ldk,bipnip-agent/lnd,bipnip-agent/ldk --all-targets --target-dir "$C/target-ws" -- -D warnings
cargo test --locked --manifest-path "$HERE/../../Cargo.toml" -p bipnip-ln --features lnd,ldk --target-dir "$C/target-ws" > "$W/unit.log" 2>&1 \
  || { tail -40 "$W/unit.log"; exit 1; }
grep -E '^test result' "$W/unit.log"

log "build the conformance programs"
cargo clippy --quiet --locked --manifest-path "$HERE/Cargo.toml" --target-dir "$C/target" -- -D warnings
cargo build --quiet --locked --release --manifest-path "$HERE/Cargo.toml" --target-dir "$C/target"

log "LDK Server at $LDK_SERVER_REV: fetch and build"
git clone -q https://github.com/lightningdevkit/ldk-server "$W/ldk-server"
git -C "$W/ldk-server" checkout -q "$LDK_SERVER_REV"
[ "$(git -C "$W/ldk-server" rev-parse HEAD)" = "$LDK_SERVER_REV" ] || fail "not the pinned LDK Server revision"
( cd "$W/ldk-server" && cargo build --quiet --locked -p ldk-server --target-dir "$C/target-ldk" )
LDK_SERVER=$C/target-ldk/debug/ldk-server

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

log "LND: alice and bob"
declare -A RPC=([alice]=10009 [bob]=10010) REST=([alice]=8081 [bob]=8082) P2P=([alice]=9735 [bob]=9736)
for n in alice bob; do
  mkdir -p "$W/$n"
  lnd --lnddir="$W/$n" --bitcoin.regtest --bitcoin.node=bitcoind \
    --bitcoind.rpchost=127.0.0.1:18443 --bitcoind.rpcuser=itest --bitcoind.rpcpass=itest \
    --bitcoind.zmqpubrawblock=tcp://127.0.0.1:28332 --bitcoind.zmqpubrawtx=tcp://127.0.0.1:28333 \
    --noseedbackup --rpclisten=127.0.0.1:${RPC[$n]} --restlisten=127.0.0.1:${REST[$n]} \
    --listen=127.0.0.1:${P2P[$n]} --alias="$n" --trickledelay=1000 > "$W/$n/lnd.log" 2>&1 &
done
lnc() { local n=$1; shift; lncli --lnddir="$W/$n" --network=regtest --rpcserver=127.0.0.1:${RPC[$n]} "$@"; }
synced() { [ "$(lnc "$1" getinfo | jget synced_to_chain)" = "True" ]; }
for n in alice bob; do wait_until 120 synced "$n" || { tail -20 "$W/$n/lnd.log"; fail "$n did not sync"; }; done
mac() { echo "$W/$1/data/chain/bitcoin/regtest/admin.macaroon"; }

log "LDK Server: dave and erin"
declare -A GRPC=([dave]=3536 [erin]=3537) LP2P=([dave]=9738 [erin]=9739)
for n in dave erin; do
  mkdir -p "$W/$n"
  cat > "$W/$n/config.toml" <<CONF
[node]
network = "regtest"
listening_addresses = ["127.0.0.1:${LP2P[$n]}"]
announcement_addresses = ["127.0.0.1:${LP2P[$n]}"]
grpc_service_address = "127.0.0.1:${GRPC[$n]}"
alias = "$n"

[storage.disk]
dir_path = "$W/$n/data"

[log]
level = "Debug"
log_to_file = true

[bitcoind]
rpc_address = "127.0.0.1:18443"
rpc_user = "itest"
rpc_password = "itest"
CONF
  "$LDK_SERVER" "$W/$n/config.toml" > "$W/$n/server.log" 2>&1 &
done
ldk_cert() { echo "$W/$1/data/tls.crt"; }
ldk_mac() { echo "$W/$1/data/regtest/macaroons/admin.macaroon"; }
ldkw() { local n=$1; shift; LDK_ADDR=127.0.0.1:${GRPC[$n]} LDK_CERT=$(ldk_cert "$n") LDK_MAC=$(ldk_mac "$n") "$C/target/release/ldk-wallet" "$@"; }
ldk_up() { test -s "$(ldk_cert "$1")" && test -s "$(ldk_mac "$1")" && ldkw "$1" id; }
for n in dave erin; do
  wait_until 120 ldk_up "$n" || { tail -40 "$W/$n/server.log" | cut -c1-300; fail "$n did not come up"; }
done
ldk_dump() { for n in dave erin; do tail -"${1:-40}" "$W/$n/server.log" | cut -c1-300 | sed "s/^/  $n: /"; done; }

# dave too: an LDK node takes an inbound anchor channel only with an on-chain reserve of its own.
log "fund alice (payer of run 1), erin (payer of run 2), bob (the lease's seller) and dave"
$BCLI -rpcwallet=miner sendtoaddress "$(ldkw dave address)" 1 >/dev/null
$BCLI -rpcwallet=miner sendtoaddress "$(lnc alice newaddress p2wkh | jget address)" 5 >/dev/null
$BCLI -rpcwallet=miner sendtoaddress "$(lnc bob newaddress p2wkh | jget address)" 5 >/dev/null
$BCLI -rpcwallet=miner sendtoaddress "$(ldkw erin address)" 5 >/dev/null
$BCLI generatetoaddress 6 "$MADDR" >/dev/null
lnd_funded() { [ "$(lnc "$1" walletbalance | jget confirmed_balance)" -gt 0 ]; }
ldk_funded() { [ "$(ldkw "$1" spendable)" -gt 0 ]; }
wait_until 60 lnd_funded alice; wait_until 60 lnd_funded bob
for n in dave erin; do wait_until 180 ldk_funded "$n" || { ldk_dump; fail "$n's wallet did not see its funds"; }; done
pair() { python3 -c 'import os,hashlib; p=os.urandom(32); print(p.hex()+":"+hashlib.sha256(p).hexdigest())'; }

log "conformance 1: LND pays (alice), LDK is paid (dave)"
# A close command for the conformance run: closes, cooperatively from alice's side, the channel
# whose funding outpoint is $1.
cat > "$W/close-alice.sh" <<CLOSE
#!/usr/bin/env bash
set -euo pipefail
exec "$(command -v lncli)" --lnddir="$W/alice" --network=regtest --rpcserver=127.0.0.1:${RPC[alice]} \
  closechannel --funding_txid "\${1%%:*}" --output_index "\${1##*:}" >/dev/null
CLOSE
chmod +x "$W/close-alice.sh"
A_BACKEND=lnd A_ADDR=https://127.0.0.1:${RPC[alice]} A_CERT=$W/alice/tls.cert A_MAC=$(mac alice) \
B_BACKEND=ldk B_ADDR=127.0.0.1:${GRPC[dave]} B_CERT=$(ldk_cert dave) B_MAC=$(ldk_mac dave) \
B_P2P=127.0.0.1:${LP2P[dave]} HASHES="$(pair),$(pair),$(pair)" \
MINE="$W/mine.sh" CLOSE="$W/close-alice.sh" \
  "$C/target/release/ln-conformance" || { ldk_dump 60; fail "conformance with an LDK payee"; }

log "conformance 2: LDK pays (erin), LND is paid (bob)"
A_BACKEND=ldk A_ADDR=127.0.0.1:${GRPC[erin]} A_CERT=$(ldk_cert erin) A_MAC=$(ldk_mac erin) \
B_BACKEND=lnd B_ADDR=https://127.0.0.1:${RPC[bob]} B_CERT=$W/bob/tls.cert B_MAC=$(mac bob) \
B_P2P=127.0.0.1:${P2P[bob]} HASHES="$(pair),$(pair),$(pair)" \
MINE="$W/mine.sh" \
  "$C/target/release/ln-conformance" || { ldk_dump 60; fail "conformance with an LDK payer"; }

log "lease end to end: an LDK buyer (erin), an LND seller (bob)"
A_BACKEND=ldk A_ADDR=127.0.0.1:${GRPC[erin]} A_CERT=$(ldk_cert erin) A_MAC=$(ldk_mac erin) \
B_BACKEND=lnd B_ADDR=https://127.0.0.1:${RPC[bob]} B_CERT=$W/bob/tls.cert B_MAC=$(mac bob) \
A_P2P=127.0.0.1:${LP2P[erin]} MINE="$W/mine.sh" \
  "$C/target/release/lease-e2e" || { ldk_dump 60; fail "a lease with an LDK buyer"; }

log "LDK E2E PASS: the conformance steps with LND paying an LDK Server and an LDK Server paying LND, and a whole lease bought by an LDK node from an LND node"
