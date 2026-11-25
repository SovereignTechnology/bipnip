#!/usr/bin/env bash
# bipnip-ln conformance for the Core Lightning backend, in a throwaway Kata VM with network, as
# root:
#
#   kata-run --cwd ~/Projects/bipnip --net --timeout 3300 -- bash itest/ln/run-cln.sh
#
# Starts regtest bitcoind, two LND nodes and two Core Lightning nodes with the `hold` plugin, all
# from sha256-pinned releases, and runs the same programs across the two kinds:
#
#   1. `ln-conformance`, LND pays and Core Lightning is paid (alice -> carol): every payee-side
#      step, hold invoices through the plugin included;
#   2. `ln-conformance`, Core Lightning pays and LND is paid (frank -> bob): every payer-side step;
#   3. `lease-e2e`, an LND buyer and a Core Lightning seller (alice, carol);
#   4. `lease-e2e`, a Core Lightning buyer and an LND seller (frank, bob).
#
# Every key and credential here is a throwaway regtest value inside the VM.

set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
W=${ITEST_WORK:-/tmp/bipnip-cln-itest}
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
# The Ubuntu 22.04 build (glibc 2.35), from the release's SHA256SUMS-v26.06.10.
CLN_SHA=e1e0a7e89b55060e2470d8568ba535a3124d6823c628def548cadeef4279c491
# BoltzExchange/hold. The project publishes no checksum file; this is the digest of the release
# asset as read on 2026-11-17 (spikes/cln-m3).
HOLD_V=v0.3.3
HOLD_SHA=f9d916430fb70a3d75089f2d1b88d5d71d4987ef79933035978c1f44aab84be5

log "tools"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq protobuf-compiler python3 curl xz-utils ca-certificates libpq5 libsqlite3-0 libsodium23 zlib1g >/dev/null

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

log "unit tests and clippy of bipnip-ln and the agent with the lnd, ldk and cln features"
cargo clippy --quiet --locked --manifest-path "$HERE/../../Cargo.toml" -p bipnip-ln -p bipnip-agent --features bipnip-ln/lnd,bipnip-ln/ldk,bipnip-ln/cln,bipnip-agent/lnd,bipnip-agent/ldk,bipnip-agent/cln --all-targets --target-dir "$C/target-ws" -- -D warnings
cargo test --locked --manifest-path "$HERE/../../Cargo.toml" -p bipnip-ln --features lnd,ldk,cln --target-dir "$C/target-ws" > "$W/unit.log" 2>&1 \
  || { tail -40 "$W/unit.log"; exit 1; }
grep -E '^test result' "$W/unit.log"

log "build the conformance programs"
cargo clippy --quiet --locked --manifest-path "$HERE/Cargo.toml" --target-dir "$C/target" -- -D warnings
cargo build --quiet --locked --release --manifest-path "$HERE/Cargo.toml" --target-dir "$C/target"

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

log "Core Lightning with the hold plugin: carol and frank"
declare -A CP2P=([carol]=9741 [frank]=9742) CGRPC=([carol]=9751 [frank]=9752)
start_cln() { # name, extra options...
  local n=$1; shift
  lightningd --network=regtest --lightning-dir="$W/$n" --daemon --log-file="$W/$n/log" \
    --bitcoin-rpcconnect=127.0.0.1 --bitcoin-rpcport=18443 --bitcoin-rpcuser=itest \
    --bitcoin-rpcpassword=itest --bitcoin-cli="$W/bin/bitcoin-cli" \
    --addr=127.0.0.1:${CP2P[$n]} --alias="$n" --grpc-port=${CGRPC[$n]} \
    --plugin="$HOLD" --hold-grpc-port=-1 "$@"
}
clc() { local n=$1; shift; lightning-cli --network=regtest --lightning-dir="$W/$n" "$@"; }
cln_sock() { echo "$W/$1/regtest/lightning-rpc"; }
cln_dump() { for n in carol frank; do tail -"${1:-40}" "$W/$n/log" 2>/dev/null | cut -c1-300 | sed "s/^/  $n: /"; done; }
for n in carol frank; do
  mkdir -p "$W/$n"
  # The developer switches only make a regtest run quicker (the chain is polled every second and
  # gossip is not held back); without them the run is the same, slower.
  start_cln "$n" --developer --dev-fast-gossip --dev-bitcoind-poll=1 > "$W/$n/start.out" 2>&1 \
    || start_cln "$n" > "$W/$n/start.out" 2>&1 \
    || { cat "$W/$n/start.out"; cln_dump 60; fail "$n did not start"; }
  wait_until 60 clc "$n" getinfo || { cln_dump 60; fail "$n does not answer"; }
  clc "$n" plugin list | grep -q "hold-linux-amd64" || fail "$n runs without the hold plugin"
done

log "fund alice (payer of run 1), frank (payer of run 2), bob and carol (the two sellers)"
$BCLI -rpcwallet=miner sendtoaddress "$(lnc alice newaddress p2wkh | jget address)" 5 >/dev/null
$BCLI -rpcwallet=miner sendtoaddress "$(lnc bob newaddress p2wkh | jget address)" 5 >/dev/null
for n in carol frank; do $BCLI -rpcwallet=miner sendtoaddress "$(clc "$n" newaddr p2tr | jget p2tr)" 5 >/dev/null; done
$BCLI generatetoaddress 6 "$MADDR" >/dev/null
lnd_funded() { [ "$(lnc "$1" walletbalance | jget confirmed_balance)" -gt 0 ]; }
cln_funded() { [ "$(clc "$1" listfunds | python3 -c 'import json,sys; print(sum(1 for o in json.load(sys.stdin)["outputs"] if o["status"] == "confirmed"))')" -gt 0 ]; }
wait_until 60 lnd_funded alice; wait_until 60 lnd_funded bob
for n in carol frank; do wait_until 120 cln_funded "$n" || { cln_dump; fail "$n's wallet did not see its funds"; }; done
pair() { python3 -c 'import os,hashlib; p=os.urandom(32); print(p.hex()+":"+hashlib.sha256(p).hexdigest())'; }

log "conformance 1: LND pays (alice), Core Lightning is paid (carol)"
# A close command for the conformance run: closes, cooperatively from alice's side, the channel
# whose funding outpoint is $1.
cat > "$W/close-alice.sh" <<CLOSE
#!/usr/bin/env bash
set -euo pipefail
exec "$(command -v lncli)" --lnddir="$W/alice" --network=regtest --rpcserver=127.0.0.1:${RPC[alice]} \
  closechannel --funding_txid "\${1%%:*}" --output_index "\${1##*:}" >/dev/null
CLOSE
chmod +x "$W/close-alice.sh"
# A close command for the conformance run: closes, from frank's side, the channel whose funding
# outpoint is $1 (Core Lightning closes by channel id when a peer has two channels). A mutual close
# not done within 60 seconds becomes a unilateral one: `close` otherwise waits 48 hours.
cat > "$W/close-frank.sh" <<CLOSE
#!/usr/bin/env bash
set -euo pipefail
lc() { "$(command -v lightning-cli)" --network=regtest --lightning-dir="$W/frank" "\$@"; }
cid=\$(lc listpeerchannels | python3 -c 'import json,sys
txid, out = sys.argv[1].split(":")
print(next(c["channel_id"] for c in json.load(sys.stdin)["channels"]
           if c.get("funding_txid") == txid and c.get("funding_outnum") == int(out)))' "\$1")
lc close "\$cid" 60 >/dev/null
CLOSE
chmod +x "$W/close-frank.sh"
A_BACKEND=lnd A_ADDR=https://127.0.0.1:${RPC[alice]} A_CERT=$W/alice/tls.cert A_MAC=$(mac alice) \
B_BACKEND=cln B_ADDR=$(cln_sock carol) \
B_P2P=127.0.0.1:${CP2P[carol]} HASHES="$(pair),$(pair),$(pair)" \
MINE="$W/mine.sh" CLOSE="$W/close-alice.sh" \
  "$C/target/release/ln-conformance" || { cln_dump 60; fail "conformance with a Core Lightning payee"; }

log "conformance 2: Core Lightning pays (frank), LND is paid (bob)"
A_BACKEND=cln A_ADDR=$(cln_sock frank) \
B_BACKEND=lnd B_ADDR=https://127.0.0.1:${RPC[bob]} B_CERT=$W/bob/tls.cert B_MAC=$(mac bob) \
B_P2P=127.0.0.1:${P2P[bob]} HASHES="$(pair),$(pair),$(pair)" \
MINE="$W/mine.sh" CLOSE="$W/close-frank.sh" \
  "$C/target/release/ln-conformance" || { cln_dump 60; fail "conformance with a Core Lightning payer"; }

log "lease end to end: an LND buyer (alice), a Core Lightning seller (carol)"
A_BACKEND=lnd A_ADDR=https://127.0.0.1:${RPC[alice]} A_CERT=$W/alice/tls.cert A_MAC=$(mac alice) \
B_BACKEND=cln B_ADDR=$(cln_sock carol) \
A_P2P=127.0.0.1:${P2P[alice]} MINE="$W/mine.sh" \
  "$C/target/release/lease-e2e" || { cln_dump 60; fail "a lease with a Core Lightning seller"; }

log "lease end to end: a Core Lightning buyer (frank), an LND seller (bob)"
A_BACKEND=cln A_ADDR=$(cln_sock frank) \
B_BACKEND=lnd B_ADDR=https://127.0.0.1:${RPC[bob]} B_CERT=$W/bob/tls.cert B_MAC=$(mac bob) \
A_P2P=127.0.0.1:${CP2P[frank]} MINE="$W/mine.sh" \
  "$C/target/release/lease-e2e" || { cln_dump 60; fail "a lease with a Core Lightning buyer"; }

log "CLN E2E PASS: the conformance steps with LND paying Core Lightning and Core Lightning paying LND (hold invoices through the hold plugin), and whole leases sold by a Core Lightning node to an LND node and by an LND node to a Core Lightning node"
