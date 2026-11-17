#!/usr/bin/env bash
# bipnip Phase 3 spike: Core Lightning with the `hold` plugin (BoltzExchange/hold), for the
# LightningBackend trait. Runs in a throwaway Kata VM with network, as root:
#
#   kata-run --cwd ~/Projects/bipnip --net --timeout 3000 -- bash spikes/cln-m3/run.sh
#
# Regtest: Bitcoin Core, one LND node (alice) and one Core Lightning node (carol) from
# sha256-pinned releases. It asserts little: it prints what the node and the plugin say.
#   1. Does the prebuilt Core Lightning run in this VM, and does `hold` load on it?
#   2. A hold invoice on a hash the payer chose: its lock time and expiry; its state before it is
#      paid, while held, after a settle with the payer's preimage, after a cancel.
#   3. Cancel before anyone paid, then pay; settle with nothing held.
#   4. A restart of Core Lightning while a payment is held.
#   5. Paying from Core Lightning: a plain invoice, a hold invoice while held and once settled, a
#      second `pay` of the same invoice, one that is cancelled.
#   6. Channels both ways, announced: what `fundchannel` returns and what `listpeerchannels`
#      shows at each end; the graph; `signmessage`.
# Every key, credential and invoice here is a throwaway regtest value inside the VM.

set -euo pipefail
W=${SPIKE_WORK:-/tmp/bipnip-cln-spike}
rm -rf "$W"; mkdir -p "$W"/{dl,bin,btc,alice,carol,cln}
log() { printf '[cln %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
say() { cut -c1-1200 | sed "s/^/    /"; }
flat() { tr -d '\n' | tr -s ' '; echo; }
jget() { python3 -c 'import json,sys
d=json.load(sys.stdin)
for k in sys.argv[1:]: d=d[int(k)] if isinstance(d,list) else d[k]
print(d)' "$@"; }
wait_until() { local secs=$1; shift; local t=0; until "$@" >/dev/null 2>&1; do sleep 1; t=$((t+1)); [ "$t" -lt "$secs" ] || return 1; done; }
newhash() { # sets P (preimage) and H (its hash)
  P=$(python3 -c 'import os;print(os.urandom(32).hex())')
  H=$(python3 -c 'import hashlib,sys;print(hashlib.sha256(bytes.fromhex(sys.argv[1])).hexdigest())' "$P")
}

BTC_V=30.4
BTC_SHA=051a18c595d9a27df23cb7187ff9fb343e90943cb142af751e12787806500c3d
LND_V=v0.21.5-beta
LND_SHA=2127d4794ea9cbd98e762ba16283615158743646d2d6d2a5bf88664dd6255ce7
CLN_V=v26.06.10
# The Ubuntu 22.04 build (glibc 2.35), from the release's SHA256SUMS-v26.06.10.
CLN_SHA=e1e0a7e89b55060e2470d8568ba535a3124d6823c628def548cadeef4279c491
HOLD_V=v0.3.3
# The project publishes no checksum file; this is the digest of the release asset as read on
# 2026-11-17.
HOLD_SHA=f9d916430fb70a3d75089f2d1b88d5d71d4987ef79933035978c1f44aab84be5
fetch() { curl -fsSL --retry 3 -o "$3" "$1"; echo "$2  $3" | sha256sum -c --quiet; }

log "tools"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq python3 curl xz-utils ca-certificates libpq5 libsqlite3-0 libsodium23 zlib1g >/dev/null
ldd --version | head -1 | say

log "downloads (sha256-pinned)"
fetch "https://bitcoincore.org/bin/bitcoin-core-$BTC_V/bitcoin-$BTC_V-x86_64-linux-gnu.tar.gz" "$BTC_SHA" "$W/dl/btc.tgz"
fetch "https://github.com/lightningnetwork/lnd/releases/download/$LND_V/lnd-linux-amd64-$LND_V.tar.gz" "$LND_SHA" "$W/dl/lnd.tgz"
fetch "https://github.com/ElementsProject/lightning/releases/download/$CLN_V/clightning-$CLN_V-Ubuntu-22.04-amd64.tar.xz" "$CLN_SHA" "$W/dl/cln.txz"
fetch "https://github.com/BoltzExchange/hold/releases/download/$HOLD_V/hold-linux-amd64.tar.gz" "$HOLD_SHA" "$W/dl/hold.tgz"
tar xzf "$W/dl/btc.tgz" -C "$W/dl"; cp "$W/dl/bitcoin-$BTC_V/bin/"{bitcoind,bitcoin-cli} "$W/bin/"
tar xzf "$W/dl/lnd.tgz" -C "$W/dl"; cp "$W/dl/lnd-linux-amd64-$LND_V/"{lnd,lncli} "$W/bin/"
tar xJf "$W/dl/cln.txz" -C "$W/cln"
tar xzf "$W/dl/hold.tgz" -C "$W/dl"
HOLD=$(find "$W/dl" -type f -name 'hold-linux-amd64' | head -1); chmod +x "$HOLD"
CLN_BIN=$(dirname "$(find "$W/cln" -type f -name lightningd | head -1)")
export PATH=$W/bin:$CLN_BIN:$PATH
log "Q1: does the prebuilt Core Lightning run here?"
{ lightningd --version 2>&1 || echo "lightningd does not run: exit $?"; } | say
{ ldd "$HOLD" 2>&1 | grep -i "not found" || echo "hold: every shared library is found"; } | say

log "bitcoind regtest"
cat > "$W/btc/bitcoin.conf" <<CONF
regtest=1
server=1
fallbackfee=0.0002
[regtest]
rpcuser=spike
rpcpassword=spike
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
mine() { $BCLI generatetoaddress "$1" "$MADDR" >/dev/null; }
mine 101

log "LND alice"
lnd --lnddir="$W/alice" --bitcoin.regtest --bitcoin.node=bitcoind \
  --bitcoind.rpchost=127.0.0.1:18443 --bitcoind.rpcuser=spike --bitcoind.rpcpass=spike \
  --bitcoind.zmqpubrawblock=tcp://127.0.0.1:28332 --bitcoind.zmqpubrawtx=tcp://127.0.0.1:28333 \
  --noseedbackup --rpclisten=127.0.0.1:10009 --restlisten=127.0.0.1:8081 \
  --listen=127.0.0.1:9735 --alias=alice --trickledelay=1000 > "$W/alice/lnd.log" 2>&1 &
lnc() { lncli --lnddir="$W/alice" --network=regtest --rpcserver=127.0.0.1:10009 "$@"; }
synced() { [ "$(lnc getinfo | jget synced_to_chain)" = "True" ]; }
wait_until 120 synced || { log "alice did not sync"; tail -20 "$W/alice/lnd.log"; exit 1; }
ALICE=$(lnc getinfo | jget identity_pubkey)

log "Core Lightning carol, with the hold plugin"
start_carol() {
  lightningd --network=regtest --lightning-dir="$W/carol" --daemon --log-file="$W/carol/log" \
    --bitcoin-rpcconnect=127.0.0.1 --bitcoin-rpcport=18443 --bitcoin-rpcuser=spike \
    --bitcoin-rpcpassword=spike --bitcoin-cli="$W/bin/bitcoin-cli" \
    --addr=127.0.0.1:9737 --alias=carol --dev-fast-gossip \
    --plugin="$HOLD" "$@"
}
clc() { lightning-cli --network=regtest --lightning-dir="$W/carol" "$@"; }
c() { { clc "$@" 2>&1 || echo "(exit $?)"; } | flat | say; }
carol_log() { tail -"${1:-30}" "$W/carol/log" | cut -c1-300 | sed 's/^/    /'; }
if ! start_carol > "$W/carol/start.out" 2>&1; then
  log "with --dev-fast-gossip it did not start: $(tail -c 300 "$W/carol/start.out" | tr '\n' ' ')"
  start_carol() {
    lightningd --network=regtest --lightning-dir="$W/carol" --daemon --log-file="$W/carol/log" \
      --bitcoin-rpcconnect=127.0.0.1 --bitcoin-rpcport=18443 --bitcoin-rpcuser=spike \
      --bitcoin-rpcpassword=spike --bitcoin-cli="$W/bin/bitcoin-cli" \
      --addr=127.0.0.1:9737 --alias=carol \
      --plugin="$HOLD" "$@"
  }
  start_carol > "$W/carol/start.out" 2>&1 || { log "Core Lightning did not start"; cat "$W/carol/start.out" | say; carol_log 60; exit 1; }
fi
wait_until 60 clc getinfo || { log "Core Lightning does not answer"; carol_log 60; exit 1; }
CAROL=$(clc getinfo | jget id)
echo "    version $(clc getinfo | jget version), height $(clc getinfo | jget blockheight)"
echo "    plugins: $(clc plugin list | python3 -c 'import json,sys; print(sorted(p["name"].split("/")[-1] for p in json.load(sys.stdin)["plugins"] if p["active"]))')"
echo "    hold's commands: $(clc help | python3 -c 'import json,sys; print(sorted(h["command"] for h in json.load(sys.stdin)["help"] if "hold" in h["command"]))')"
{ ls "$W/carol/regtest" 2>&1 || true; } | tr '\n' ' ' | say; echo
{ ls "$W/carol/regtest/hold" 2>&1 || true; } | tr '\n' ' ' | say; echo
c signmessage "bipnip cln probe"

log "fund alice and carol"
$BCLI -rpcwallet=miner sendtoaddress "$(lnc newaddress p2wkh | jget address)" 5 >/dev/null
$BCLI -rpcwallet=miner sendtoaddress "$(clc newaddr | jget bech32)" 2 >/dev/null
mine 6
alice_funded() { [ "$(lnc walletbalance | jget confirmed_balance)" -gt 0 ]; }
carol_funded() { [ "$(clc listfunds | python3 -c 'import json,sys; print(sum(1 for o in json.load(sys.stdin)["outputs"] if o["status"] == "confirmed"))')" -gt 0 ]; }
wait_until 60 alice_funded
wait_until 120 carol_funded || { log "carol's wallet did not see its funds"; c listfunds; }

log "Q6a: an announced channel alice (LND) -> carol (Core Lightning)"
{ lnc connect "$CAROL@127.0.0.1:9737" 2>&1 || true; } | flat | say
{ lnc openchannel --node_key "$CAROL" --local_amt 1000000 2>&1 || true; } | flat | say
mine 6
alice_active() { [ "$(lnc listchannels --active_only --peer "$CAROL" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["channels"]))')" -ge "${1:-1}" ]; }
carol_normal() { [ "$(clc listpeerchannels "$ALICE" | python3 -c 'import json,sys;print(sum(1 for c in json.load(sys.stdin)["channels"] if c["state"] == "CHANNELD_NORMAL"))')" -ge "${1:-1}" ]; }
wait_until 180 alice_active || log "the channel did not become active at alice"
wait_until 180 carol_normal || log "the channel did not become normal at carol"
chan_view() { clc listpeerchannels "$ALICE" | python3 -c '
import json, sys
for c in json.load(sys.stdin)["channels"]:
    keep = ("state", "opener", "private", "funding_txid", "funding_outnum", "total_msat", "short_channel_id", "peer_connected", "status")
    print({k: c.get(k) for k in keep})' | say; }
chan_view

pay_from_alice() { : > "$2"; (lnc payinvoice --force --timeout 120s "$1" >> "$2" 2>&1 || true) & }
hstate() { clc listholdinvoices "$1" | python3 -c 'import json,sys; h=json.load(sys.stdin)["holdinvoices"]; print(h[0]["state"] if h else "none")'; }
is_state() { [ "$(hstate "$1")" = "$2" ]; }
hold_view() { clc listholdinvoices "$1" | python3 -c '
import json, sys
for h in json.load(sys.stdin)["holdinvoices"]:
    h["invoice"] = h["invoice"][:24] + "…"
    print(h)' | say; }

log "Q2: a hold invoice on a hash the payer chose"
newhash; H1=$H; P1=$P
c holdinvoice "$H1" 1500000
INV1=$(clc listholdinvoices "$H1" | jget holdinvoices 0 invoice)
clc decode "$INV1" | python3 -c 'import json,sys; d=json.load(sys.stdin); print({k: d.get(k) for k in ("payee","amount_msat","expiry","min_final_cltv_expiry","description","payment_hash","created_at")})' | say
echo "    before anyone pays: $(hstate "$H1"); height $($BCLI getblockcount)"
pay_from_alice "$INV1" "$W/pay1.out"
for attempt in 1 2 3; do
  wait_until 40 is_state "$H1" accepted && break
  log "not accepted yet (attempt $attempt): $(tail -c 200 "$W/pay1.out" | tr '\n' ' ')"
  pay_from_alice "$INV1" "$W/pay1.out"
done
echo "    while held:"; hold_view "$H1"
echo "    settle with the payer's preimage:"; c settleholdinvoice "$P1"
sleep 5; echo "    then: $(hstate "$H1"); alice: $(tr -d '\n' < "$W/pay1.out" | tail -c 200)"
echo "    settle again:"; c settleholdinvoice "$P1"
echo "    a second hold invoice on the same hash:"; c holdinvoice "$H1" 1500000

log "Q2': held, then cancelled"
newhash; H2=$H
clc holdinvoice "$H2" 1500000 > /dev/null; INV2=$(clc listholdinvoices "$H2" | jget holdinvoices 0 invoice)
pay_from_alice "$INV2" "$W/pay2.out"
wait_until 60 is_state "$H2" accepted || log "the second invoice was not accepted"
c cancelholdinvoice "$H2"
sleep 5; echo "    then: $(hstate "$H2"); alice: $(tr -d '\n' < "$W/pay2.out" | tail -c 200)"
echo "    cancel again:"; c cancelholdinvoice "$H2"

log "Q3: cancelled before anyone paid, then paid; settle with nothing held; an unknown hash"
newhash; H3=$H; P3=$P
clc holdinvoice "$H3" 1500000 > /dev/null; INV3=$(clc listholdinvoices "$H3" | jget holdinvoices 0 invoice)
c cancelholdinvoice "$H3"
pay_from_alice "$INV3" "$W/pay3.out"; sleep 12
echo "    then: $(hstate "$H3"); alice: $(tr -d '\n' < "$W/pay3.out" | tail -c 200)"
newhash; H4=$H; P4=$P
clc holdinvoice "$H4" 1500000 > /dev/null
echo "    settle with nothing held:"; c settleholdinvoice "$P4"
echo "    state of a hash nobody made an invoice for: $(hstate "$(printf 'ab%.0s' $(seq 32))")"
echo "    cancel a hash nobody made an invoice for:"; c cancelholdinvoice "$(printf 'ab%.0s' $(seq 32))"

log "Q4: Core Lightning is restarted while a payment is held"
newhash; H5=$H; P5=$P
clc holdinvoice "$H5" 1500000 > /dev/null; INV5=$(clc listholdinvoices "$H5" | jget holdinvoices 0 invoice)
pay_from_alice "$INV5" "$W/pay5.out"
wait_until 60 is_state "$H5" accepted || log "the invoice was not accepted before the restart"
clc stop > /dev/null 2>&1 || true; sleep 5
start_carol > "$W/carol/start2.out" 2>&1 || { log "Core Lightning did not restart"; cat "$W/carol/start2.out" | say; }
wait_until 60 clc getinfo || { log "Core Lightning does not answer after the restart"; carol_log 40; }
sleep 10
echo "    after the restart: $(hstate "$H5"); alice's pending HTLCs: $(lnc listchannels --peer "$CAROL" | python3 -c 'import json,sys; print(sum(len(c["pending_htlcs"]) for c in json.load(sys.stdin)["channels"]))')"
echo "    settle:"; c settleholdinvoice "$P5"
sleep 8; echo "    then: $(hstate "$H5"); alice: $(tr -d '\n' < "$W/pay5.out" | tail -c 200)"

log "Q6b: an announced channel carol (Core Lightning) -> alice (LND)"
c connect "$ALICE" 127.0.0.1 9735
c fundchannel "$ALICE" 500000 normal true
chan_view
mine 6
wait_until 180 carol_normal 2 || log "carol's own channel did not become normal"
wait_until 180 alice_active 2 || log "carol's own channel did not become active at alice"
chan_view
echo "    at alice: $(lnc listchannels --peer "$CAROL" | python3 -c 'import json,sys; print([(c["capacity"], c["initiator"], c["private"], c["active"]) for c in json.load(sys.stdin)["channels"]])')"
mine 6; sleep 20
echo "    carol's graph: alice's channels: $(clc listchannels null "$ALICE" | python3 -c 'import json,sys; print([(c["destination"][:8], c["short_channel_id"], c["amount_msat"], c["public"]) for c in json.load(sys.stdin)["channels"]])')"
echo "    carol's graph: alice as a node:"; c listnodes "$ALICE"
echo "    carol's graph: an unknown node:"; c listnodes "02$(printf '11%.0s' $(seq 32))"

pay_view() { clc listpays null "$1" | python3 -c 'import json,sys; print([{k: p.get(k) for k in ("status","preimage","amount_msat","amount_sent_msat","payment_hash")} for p in json.load(sys.stdin)["pays"]])' | say; }
pay_bg() { : > "$2"; (clc pay "$1" > "$2" 2>&1 || true) & }
log "Q5a: carol pays a plain invoice of alice's, then pays it again"
INVA=$(lnc addinvoice --amt 2000 | jget payment_request); HA=$(lnc decodepayreq "$INVA" | jget payment_hash)
c pay "$INVA"
pay_view "$HA"
echo "    again:"; c pay "$INVA"

log "Q5b: carol pays a hold invoice of alice's; while held; a second pay; settled"
newhash; H8=$H; P8=$P
INVH=$(lnc addholdinvoice "$H8" 3000 | jget payment_request)
pay_bg "$INVH" "$W/cpay8.out"; sleep 10
echo "    while held (alice: $(lnc lookupinvoice "$H8" | jget state)):"; pay_view "$H8"
echo "    a second pay while held (10 s):"; { timeout 10 lightning-cli --network=regtest --lightning-dir="$W/carol" pay "$INVH" 2>&1 || echo "(exit $?)"; } | flat | say
lnc settleinvoice "$P8" >/dev/null; sleep 8
echo "    once settled:"; pay_view "$H8"
echo "    the first pay returned: $(tr -d '\n' < "$W/cpay8.out" | tr -s ' ' | cut -c1-400)"

log "Q5c: carol pays a hold invoice that alice cancels; then pays it again"
newhash; H9=$H
INVC=$(lnc addholdinvoice "$H9" 3000 | jget payment_request)
pay_bg "$INVC" "$W/cpay9.out"; sleep 10
lnc cancelinvoice "$H9" >/dev/null; sleep 10
echo "    once cancelled:"; pay_view "$H9"
echo "    the pay returned: $(tr -d '\n' < "$W/cpay9.out" | tr -s ' ' | cut -c1-400)"
echo "    a payment nobody made:"; pay_view "$(printf 'cd%.0s' $(seq 32))"

clc stop > /dev/null 2>&1 || true
log "CLN PROBE DONE"
