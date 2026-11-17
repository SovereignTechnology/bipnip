#!/usr/bin/env bash
# bipnip Phase 3 spike: what does an LDK Server (ldk-server) answer, call by call, for the things
# the LightningBackend trait needs? Runs in a throwaway Kata VM with network, as root:
#
#   kata-run --cwd ~/Projects/bipnip --net --timeout 3300 -- bash spikes/ldk-m3/run.sh
#
# Regtest: Bitcoin Core, one LND node (alice) and one ldk-server (dave) built from a pinned
# upstream revision. It asserts little: it prints what the server says, for RESULTS.md and for
# the backend that follows. The questions:
#   1. A hold invoice (Bolt11ReceiveForHash): what lock time does its invoice ask for? Is there a
#      payment record before anyone pays, and under which id?
#   2. When the payer's HTLC arrives: which id does PaymentClaimable carry, what does the payment
#      record say, and how far away is the claim deadline?
#   3. Claim with the payer's preimage; fail; fail before anyone paid; claim with nothing to claim.
#   4. A PaymentClaimable that nobody was subscribed for: is it ever seen again? Can the payment
#      still be found, claimed or failed without it?
#   5. Paying: the id of an outbound payment, its record while held and once settled, and what a
#      second send of the same invoice answers.
#   6. Channels: does ldk-server take an announced channel from LND; does LND take one from it;
#      when does the funding outpoint show.
# Every key, credential and invoice here is a throwaway regtest value inside the VM.

set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
W=${SPIKE_WORK:-/tmp/bipnip-ldk-spike}
rm -rf "$W"; mkdir -p "$W"/{dl,bin,btc,alice,dave}
log() { printf '[ldk %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
say() { sed "s/^/    /"; }
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
LDK_SERVER_REV=d7e7a2e708664af922c53c786ad6a5df622f5dd8
fetch() { curl -fsSL --retry 3 -o "$3" "$1"; echo "$2  $3" | sha256sum -c --quiet; }

log "tools"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq python3 curl ca-certificates git cmake clang pkg-config >/dev/null

log "downloads (sha256-pinned)"
fetch "https://bitcoincore.org/bin/bitcoin-core-$BTC_V/bitcoin-$BTC_V-x86_64-linux-gnu.tar.gz" "$BTC_SHA" "$W/dl/btc.tgz"
fetch "https://github.com/lightningnetwork/lnd/releases/download/$LND_V/lnd-linux-amd64-$LND_V.tar.gz" "$LND_SHA" "$W/dl/lnd.tgz"
tar xzf "$W/dl/btc.tgz" -C "$W/dl"; cp "$W/dl/bitcoin-$BTC_V/bin/"{bitcoind,bitcoin-cli} "$W/bin/"
tar xzf "$W/dl/lnd.tgz" -C "$W/dl"; cp "$W/dl/lnd-linux-amd64-$LND_V/"{lnd,lncli} "$W/bin/"
export PATH=$W/bin:$PATH

log "ldk-server at $LDK_SERVER_REV: fetch and build (debug)"
git clone -q https://github.com/lightningdevkit/ldk-server "$W/ldk-server"
git -C "$W/ldk-server" checkout -q "$LDK_SERVER_REV"
[ "$(git -C "$W/ldk-server" rev-parse HEAD)" = "$LDK_SERVER_REV" ]
( cd "$W/ldk-server" && cargo build --quiet --locked -p ldk-server --target-dir "$W/target-ldk" )
LDK_SERVER=$W/target-ldk/debug/ldk-server
log "build the probe"
cargo build --quiet --manifest-path "$HERE/Cargo.toml" --target-dir "$W/target"
PROBE=$W/target/debug/ldk-probe

log "bitcoind regtest"
cat > "$W/btc/bitcoin.conf" <<EOF
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
EOF
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

log "ldk-server dave"
cat > "$W/dave/config.toml" <<EOF
[node]
network = "regtest"
listening_addresses = ["127.0.0.1:9738"]
announcement_addresses = ["127.0.0.1:9738"]
grpc_service_address = "127.0.0.1:3536"
alias = "dave"

[storage.disk]
dir_path = "$W/dave/data"

[log]
level = "Debug"
log_to_file = true

[bitcoind]
rpc_address = "127.0.0.1:18443"
rpc_user = "spike"
rpc_password = "spike"
EOF
"$LDK_SERVER" "$W/dave/config.toml" > "$W/dave/server.log" 2>&1 &
LDK_PID=$!
dave_log() { tail -"${1:-30}" "$W/dave/server.log" | cut -c1-300 | say; }
export LDK_ADDR=127.0.0.1:3536 LDK_CERT="$W/dave/data/tls.crt" LDK_MAC="$W/dave/data/regtest/macaroons/admin.macaroon"
up() { test -s "$LDK_CERT" && test -s "$LDK_MAC" && "$PROBE" id; }
wait_until 120 up || { log "ldk-server did not come up"; ls -R "$W/dave/data" 2>/dev/null | head -40 | say; dave_log 60; exit 1; }
# The probe prints errors as answers; a crash of the probe itself must not end the run either.
p() { { "$PROBE" "$@" 2>&1 || echo "PROBE EXIT $?"; } | cut -c1-1500 | say; }
hold() { # hash, msat, output file: prints the answer and leaves the invoice in INV
  "$PROBE" hold "$1" "$2" > "$3" 2>&1 || echo "PROBE EXIT $?" >> "$3"
  INV=$(sed -n 's/^INVOICE //p' "$3" || true)
}
DAVE=$("$PROBE" id)
log "dave is up"; p info
p sign "bipnip ldk probe"

log "fund alice and dave"
$BCLI -rpcwallet=miner sendtoaddress "$(lnc newaddress p2wkh | jget address)" 5 >/dev/null
$BCLI -rpcwallet=miner sendtoaddress "$("$PROBE" address)" 2 >/dev/null
mine 6
alice_funded() { [ "$(lnc walletbalance | jget confirmed_balance)" -gt 0 ]; }
dave_funded() { [ "$("$PROBE" spendable)" -gt 0 ]; }
wait_until 60 alice_funded
wait_until 180 dave_funded || { log "dave's wallet did not see its funds"; p balances; dave_log; }
p balances

"$PROBE" events > "$W/events.log" 2>&1 &
EVENTS_PID=$!
events_since() { tail -n +"$(( $1 + 1 ))" "$W/events.log"; }
mark() { wc -l < "$W/events.log"; }

log "Q6a: an announced channel alice (LND) -> dave (ldk-server)"
lnc connect "$DAVE@127.0.0.1:9738" 2>&1 | say || true
if lnc openchannel --node_key "$DAVE" --local_amt 1000000 > "$W/open-a-d.json" 2>&1; then
  echo "    LND opened it: $(tr -d '\n ' < "$W/open-a-d.json" | cut -c1-200)"
else
  echo "    LND could not open an announced channel: $(tr -d '\n' < "$W/open-a-d.json" | cut -c1-300)"
  log "Q6a': the same, unannounced"
  { lnc openchannel --node_key "$DAVE" --local_amt 1000000 --private 2>&1 || true; } | tr -d '\n ' | cut -c1-200 | say; echo
fi
p channels
mine 6
alice_active() { [ "$(lnc listchannels --active_only --peer "$DAVE" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["channels"]))')" -ge 1 ]; }
dave_usable() { out=$("$PROBE" channels 2>&1); grep -q "usable=true" <<<"$out"; }
wait_until 180 alice_active || log "the channel did not become active at alice"
wait_until 180 dave_usable || log "the channel did not become usable at dave"
p channels

pay_from_alice() { # invoice, log file: pays in the background (a held payment blocks payinvoice)
  : > "$2"
  (lnc payinvoice --force --timeout 120s "$1" >> "$2" 2>&1 || true) &
}
claimable_seen() { out=$(events_since "$1"); grep -q "PaymentClaimable" <<<"$out"; }
claimable_id() { { events_since "$1" | grep -m1 "PaymentClaimable" | sed -n 's/.*PaymentClaimable(PaymentClaimable { payment_id: "\([0-9a-f]*\)".*/\1/p'; } || true; }

log "Q1: a hold invoice on a hash the payer chose; before anyone pays"
newhash; H1=$H; P1=$P
hold "$H1" 1500000 "$W/hold1.out"; INV1=$INV; cut -c1-1500 "$W/hold1.out" | say
echo "    best block: $($BCLI getblockcount)"
echo "    record under the payment hash as id:"; p details "$H1"
echo "    all payments:"; p payments

log "Q2: alice pays it; what dave says while it is held"
M=$(mark); pay_from_alice "$INV1" "$W/pay1.out"
for attempt in 1 2 3; do
  wait_until 40 claimable_seen "$M" && break
  log "no PaymentClaimable yet (attempt $attempt): $(tail -c 200 "$W/pay1.out" | tr '\n' ' ')"
  pay_from_alice "$INV1" "$W/pay1.out"
done
events_since "$M" | cut -c1-1500 | say
ID1=$(claimable_id "$M")
echo "    payment id in the event: ${ID1:-none}; the payment hash: $H1; same: $([ "$ID1" = "$H1" ] && echo yes || echo no)"
echo "    record under the event's id:"; p details "${ID1:-$H1}"
echo "    record under the hash:"; p details "$H1"
echo "    best block: $($BCLI getblockcount)"

log "Q3a: claim with the payer's preimage"
M=$(mark); p claim "${ID1:-$H1}" "$P1" 1500000
sleep 5; events_since "$M" | cut -c1-800 | say
echo "    record after the claim:"; p details "${ID1:-$H1}"
echo "    alice's payment: $(tr -d '\n' < "$W/pay1.out" | tail -c 300)"
echo "    claim again (a retry after a lost answer):"; p claim "${ID1:-$H1}" "$P1" 1500000

log "Q3b: a second hold invoice, failed while held"
newhash; H2=$H
hold "$H2" 1500000 "$W/hold2.out"; INV2=$INV
M=$(mark); pay_from_alice "$INV2" "$W/pay2.out"
wait_until 60 claimable_seen "$M" || log "no PaymentClaimable for the second invoice"
ID2=$(claimable_id "$M")
p fail "${ID2:-$H2}"
sleep 5; events_since "$M" | cut -c1-800 | say
echo "    record after the fail:"; p details "${ID2:-$H2}"
echo "    alice's payment: $(tr -d '\n' < "$W/pay2.out" | tail -c 300)"
echo "    fail again:"; p fail "${ID2:-$H2}"

log "Q3c: a hold invoice failed before anyone paid (by its hash), then paid"
newhash; H3=$H
hold "$H3" 1500000 "$W/hold3.out"; INV3=$INV
p fail "$H3"
echo "    record:"; p details "$H3"
M=$(mark); pay_from_alice "$INV3" "$W/pay3.out"; sleep 15
events_since "$M" | cut -c1-800 | say
echo "    alice's payment: $(tr -d '\n' < "$W/pay3.out" | tail -c 300)"
echo "    record:"; p details "$H3"

log "Q3d: claiming a hold invoice nobody has paid"
newhash; H4=$H; P4=$P
hold "$H4" 1500000 "$W/hold4.out"
p claim "$H4" "$P4"

log "Q4: a PaymentClaimable while nobody is subscribed"
kill "$EVENTS_PID"; wait "$EVENTS_PID" 2>/dev/null || true
newhash; H6=$H; P6=$P
hold "$H6" 1500000 "$W/hold6.out"; INV6=$INV
pay_from_alice "$INV6" "$W/pay6.out"; sleep 20
echo "    alice's payment so far: $(tr -d '\n' < "$W/pay6.out" | tail -c 200)"
echo "    alice's pending HTLCs: $(lnc listchannels --peer "$DAVE" | python3 -c 'import json,sys; print(sum(len(c["pending_htlcs"]) for c in json.load(sys.stdin)["channels"]))')"
echo "    record under the hash, with no subscriber:"; p details "$H6"
echo "    all payments:"; { "$PROBE" payments 2>&1 | tr '}' '\n' | grep -c "payment_id" || true; } | sed 's/^/    count: /'
"$PROBE" events > "$W/events.log" 2>&1 &
EVENTS_PID=$!
sleep 10
echo "    events after subscribing again:"; cut -c1-800 "$W/events.log" | say
echo "    claim by the hash as id, without ever having seen the event:"; p claim "$H6" "$P6" 1500000
sleep 5
echo "    record:"; p details "$H6"
echo "    alice's payment: $(tr -d '\n' < "$W/pay6.out" | tail -c 300)"

log "Q4': the same, and ldk-server is restarted while the payment is held"
newhash; H7=$H; P7=$P
kill "$EVENTS_PID"; wait "$EVENTS_PID" 2>/dev/null || true
hold "$H7" 1500000 "$W/hold7.out"; INV7=$INV
pay_from_alice "$INV7" "$W/pay7.out"; sleep 20
kill "$LDK_PID"; wait "$LDK_PID" 2>/dev/null || true
"$LDK_SERVER" "$W/dave/config.toml" >> "$W/dave/server.log" 2>&1 &
LDK_PID=$!
wait_until 120 up || { log "ldk-server did not come back"; dave_log 60; exit 1; }
"$PROBE" events > "$W/events.log" 2>&1 &
EVENTS_PID=$!
sleep 30
echo "    events after the restart:"; cut -c1-800 "$W/events.log" | say
echo "    record under the hash:"; p details "$H7"
ID7=$(claimable_id 0)
echo "    fail it (id ${ID7:-$H7}):"; p fail "${ID7:-$H7}"
sleep 5; echo "    alice's payment: $(tr -d '\n' < "$W/pay7.out" | tail -c 300)"

log "Q6b: an announced channel dave (ldk-server) -> alice (LND)"
wait_until 120 dave_usable || log "dave's first channel is not usable after the restart"
p connect "$ALICE" 127.0.0.1:9735
p open "$ALICE" 127.0.0.1:9735 500000 announced
for i in 1 2 3 4 5; do { "$PROBE" channels 2>&1 | grep "outbound=true" || echo "no outbound channel listed yet"; } | cut -c1-400 | say; sleep 2; done
mine 6
dave_two() { out=$("$PROBE" channels 2>&1); [ "$(grep -c "usable=true" <<<"$out")" -ge 2 ]; }
wait_until 180 dave_two || log "dave's own channel did not become usable"
p channels
echo "    at alice: $(lnc listchannels --peer "$DAVE" | python3 -c 'import json,sys; print([(c["capacity"], c["initiator"], c["private"], c["active"]) for c in json.load(sys.stdin)["channels"]])')"
mine 6; sleep 20
echo "    dave's graph view of alice:"; p graph "$ALICE"
echo "    dave's graph view of an unknown node:"; p graph "02$(printf '11%.0s' $(seq 32))"

log "Q5a: dave pays a plain invoice of alice's"
INVA=$(lnc addinvoice --amt 2000 | jget payment_request)
HA=$(lnc decodepayreq "$INVA" | jget payment_hash)
"$PROBE" pay "$INVA" 10000 > "$W/dpay1.out" 2>&1 || true; cut -c1-400 "$W/dpay1.out" | say
IDA=$(sed -n 's/.*payment_id: "\([0-9a-f]*\)".*/\1/p' "$W/dpay1.out" | head -1 || true)
echo "    outbound payment id: ${IDA:-none}; the payment hash: $HA; same: $([ "$IDA" = "$HA" ] && echo yes || echo no)"
sleep 8; echo "    record:"; p details "${IDA:-$HA}"
echo "    paying the same invoice again:"; p pay "$INVA" 10000

log "Q5b: dave pays a hold invoice of alice's; the record while held; a second send; settled"
newhash; H8=$H; P8=$P
INVH=$(lnc addholdinvoice "$H8" 3000 | jget payment_request)
p pay "$INVH" 10000
sleep 8; echo "    record while held:"; p details "$H8"
echo "    alice's invoice state: $(lnc lookupinvoice "$H8" | jget state)"
echo "    paying it again while held:"; p pay "$INVH" 10000
lnc settleinvoice "$P8" >/dev/null
sleep 8; echo "    record once settled:"; p details "$H8"

log "Q5c: dave pays a hold invoice that alice then cancels"
newhash; H9=$H
INVC=$(lnc addholdinvoice "$H9" 3000 | jget payment_request)
p pay "$INVC" 10000
sleep 8; lnc cancelinvoice "$H9" >/dev/null
sleep 8; echo "    record once canceled:"; p details "$H9"
echo "    paying it again after it failed:"; p pay "$INVC" 10000
echo "    a record nobody made:"; p details "$(printf 'ab%.0s' $(seq 32))"

kill "$EVENTS_PID" "$LDK_PID" 2>/dev/null || true
log "LDK PROBE DONE"
