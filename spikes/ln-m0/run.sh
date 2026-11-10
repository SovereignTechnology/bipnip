#!/usr/bin/env bash
# bipnip M0 Lightning spike. Runs in a throwaway Kata VM with network (kata-run --net), as root:
#
#   kata-run --cwd ~/Projects/bipnip --net --timeout 3000 -- bash spikes/ln-m0/run.sh
#
# On regtest, with pinned releases (sha256-checked), it collects:
#   1. signmessage vectors from LND (two nodes), Core Lightning and ldk-node, and binding statements
#      signed by each, for bipnip-core's node_binding to verify (docs/nips/node-binding.md);
#   2. a Nostr event signed by LND's signer with a derived key (signrpc SignMessage, schnorr_sig),
#      to see whether a node-held key can sign Nostr events without exporting it (ADR 0004);
#   3. hold invoices on a hash the payer chose: LND (addholdinvoice) and ldk-node (receive_for_hash /
#      claim_for_hash), paid over a real channel, accepted, then settled with the payer's preimage
#      (ADR 0003). Core Lightning has no built-in hold invoices; only its signing is checked here.
# Then `check` verifies everything independently and the script exits with its status.
# Every key, credential and invoice here is a throwaway regtest value inside the VM.

set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
W=${SPIKE_WORK:-/tmp/bipnip-ln-spike}
rm -rf "$W"; mkdir -p "$W"/{dl,bin,out,btc}
OUT=$W/out
log() { printf '[spike %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
jget() { python3 -c 'import json,sys
d=json.load(sys.stdin)
for k in sys.argv[1:]: d=d[int(k)] if isinstance(d,list) else d[k]
print(d)' "$@"; }
emit() { local f=$1; shift; python3 -c 'import json,sys; print(json.dumps(dict(zip(sys.argv[1::2], sys.argv[2::2]))))' "$@" >> "$OUT/$f"; }
result() { python3 -c 'import json,sys; print(json.dumps({"check":sys.argv[1],"ok":sys.argv[2]=="ok","detail":sys.argv[3]}))' "$1" "$2" "$3" >> "$OUT/results.jsonl"; }
wait_until() { local secs=$1; shift; local t=0; until "$@" >/dev/null 2>&1; do sleep 1; t=$((t+1)); [ "$t" -lt "$secs" ] || return 1; done; }

# --- pinned releases --------------------------------------------------------------------------
BTC_V=30.4
BTC_SHA=051a18c595d9a27df23cb7187ff9fb343e90943cb142af751e12787806500c3d
LND_V=v0.21.5-beta
LND_SHA=2127d4794ea9cbd98e762ba16283615158743646d2d6d2a5bf88664dd6255ce7
CLN_V=v26.06.10
CLN_SHA=dbfb97c2a42f4446f4ac5217fff9274a3fda9996e3ea37c9c8a3e7bd8b17d200
fetch() { curl -fsSL --retry 3 -o "$3" "$1"; echo "$2  $3" | sha256sum -c --quiet; }

log "tools"
export DEBIAN_FRONTEND=noninteractive
command -v python3 >/dev/null && command -v curl >/dev/null && command -v xz >/dev/null || {
  apt-get update -qq; apt-get install -y -qq python3 curl xz-utils ca-certificates >/dev/null; }

log "downloads (sha256-pinned)"
fetch "https://bitcoincore.org/bin/bitcoin-core-$BTC_V/bitcoin-$BTC_V-x86_64-linux-gnu.tar.gz" "$BTC_SHA" "$W/dl/btc.tgz"
fetch "https://github.com/lightningnetwork/lnd/releases/download/$LND_V/lnd-linux-amd64-$LND_V.tar.gz" "$LND_SHA" "$W/dl/lnd.tgz"
tar xzf "$W/dl/btc.tgz" -C "$W/dl"; cp "$W/dl/bitcoin-$BTC_V/bin/"{bitcoind,bitcoin-cli} "$W/bin/"
tar xzf "$W/dl/lnd.tgz" -C "$W/dl"; cp "$W/dl/lnd-linux-amd64-$LND_V/"{lnd,lncli} "$W/bin/"
export PATH=$W/bin:$PATH

log "build the spike programs (ldk-node 0.7.1, checker)"
cargo build --quiet --release --manifest-path "$HERE/Cargo.toml" --target-dir "$W/target"
LDK=$W/target/release/ldk-spike; CHECK=$W/target/release/check

# --- bitcoind regtest -------------------------------------------------------------------------
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

# --- LND: alice and bob -----------------------------------------------------------------------
declare -A RPC=([alice]=10009 [bob]=10010) REST=([alice]=8081 [bob]=8082) P2P=([alice]=9735 [bob]=9736)
for n in alice bob; do
  mkdir -p "$W/$n"
  lnd --lnddir="$W/$n" --bitcoin.regtest --bitcoin.node=bitcoind \
    --bitcoind.rpchost=127.0.0.1:18443 --bitcoind.rpcuser=spike --bitcoind.rpcpass=spike \
    --bitcoind.zmqpubrawblock=tcp://127.0.0.1:28332 --bitcoind.zmqpubrawtx=tcp://127.0.0.1:28333 \
    --noseedbackup --rpclisten=127.0.0.1:${RPC[$n]} --restlisten=127.0.0.1:${REST[$n]} \
    --listen=127.0.0.1:${P2P[$n]} --alias="$n" > "$W/$n/lnd.log" 2>&1 &
done
lnc() { local n=$1; shift; lncli --lnddir="$W/$n" --network=regtest --rpcserver=127.0.0.1:${RPC[$n]} "$@"; }
synced() { [ "$(lnc "$1" getinfo | jget synced_to_chain)" = "True" ]; }
for n in alice bob; do wait_until 120 synced "$n" || { log "$n did not sync"; tail -20 "$W/$n/lnd.log"; exit 1; }; done
declare -A PK
for n in alice bob; do PK[$n]=$(lnc "$n" getinfo | jget identity_pubkey); done
log "alice ${PK[alice]} bob ${PK[bob]}"

MSG="bipnip M0 spike: Lightning signed message"
NOSTR_X=$(python3 -c 'print("ab"*32)')   # a placeholder Nostr key for binding statements
TS=$(date +%s)
stmt() { echo "nostr-node-binding:v1:bind:regtest:$NOSTR_X:$1:$TS"; }
for n in alice bob; do
  emit vectors.jsonl impl "lnd-$LND_V-$n" msg "$MSG" sig "$(lnc "$n" signmessage "$MSG" | jget signature)" pubkey "${PK[$n]}"
  S=$(stmt "${PK[$n]}")
  emit statements.jsonl impl "lnd-$LND_V-$n" statement "$S" sig "$(lnc "$n" signmessage "$S" | jget signature)"
done

# --- LND signer as a Nostr key ----------------------------------------------------------------
log "LND signer: derived key, Schnorr over a Nostr event serialization"
MAC=$(python3 -c 'import sys;print(open(sys.argv[1],"rb").read().hex())' "$W/alice/data/chain/bitcoin/regtest/admin.macaroon")
rest() { curl -fsS --cacert "$W/alice/tls.cert" -H "Grpc-Metadata-macaroon: $MAC" -X POST "https://127.0.0.1:${REST[alice]}$1" -d "$2"; }
KEY=$(rest /v2/wallet/key '{"key_family":1735,"key_index":0}' | jget raw_key_bytes)
KEY2=$(rest /v2/wallet/key '{"key_family":1735,"key_index":0}' | jget raw_key_bytes)
[ "$KEY" = "$KEY2" ] && result "LND DeriveKey is deterministic" ok "family 1735 index 0" \
  || result "LND DeriveKey is deterministic" fail "two calls differ"
python3 - "$KEY" "$W/ev.json" <<'PY'
import base64, hashlib, json, sys, time
raw = base64.b64decode(sys.argv[1]); xonly = raw[1:].hex()
ev = {"pubkey": xonly, "created_at": int(time.time()), "kind": 1, "tags": [],
      "content": "bipnip M0 spike: signed by LND's signer with a derived key"}
ser = json.dumps([0, ev["pubkey"], ev["created_at"], ev["kind"], ev["tags"], ev["content"]],
                 separators=(",", ":"), ensure_ascii=False).encode()
ev["id"] = hashlib.sha256(ser).hexdigest()
json.dump({"event": ev, "ser_b64": base64.b64encode(ser).decode(), "derived_xonly": xonly}, open(sys.argv[2], "w"))
PY
REQ=$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(json.dumps({"msg":d["ser_b64"],"key_loc":{"key_family":1735,"key_index":0},"schnorr_sig":True}))' "$W/ev.json")
SIG=$(rest /v2/signer/signmessage "$REQ" | jget signature)
python3 - "$W/ev.json" "$SIG" >> "$OUT/nostr_events.jsonl" <<'PY'
import base64, json, sys
d = json.load(open(sys.argv[1])); d["event"]["sig"] = base64.b64decode(sys.argv[2]).hex()
print(json.dumps({"impl": "lnd signrpc SignMessage schnorr_sig (family 1735)", "event": d["event"], "derived_xonly": d["derived_xonly"]}))
PY

# --- channel alice -> bob, LND hold invoice on alice's hash -----------------------------------
log "fund alice, open alice -> bob"
ADDR=$(lnc alice newaddress p2wkh | jget address)
$BCLI -rpcwallet=miner sendtoaddress "$ADDR" 5 >/dev/null; mine 6
funded() { [ "$(lnc alice walletbalance | jget confirmed_balance)" -gt 0 ]; }
wait_until 60 funded
lnc alice connect "${PK[bob]}@127.0.0.1:${P2P[bob]}" >/dev/null
lnc alice openchannel --node_key "${PK[bob]}" --local_amt 1000000 >/dev/null
mine 6
active_with() { [ "$(lnc alice listchannels --active_only --peer "$1" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["channels"]))')" -ge 1 ]; }
wait_until 120 active_with "${PK[bob]}"

log "LND hold invoice on a payer-chosen hash"
P=$(python3 -c 'import os;print(os.urandom(32).hex())')
H=$(python3 -c 'import hashlib,sys;print(hashlib.sha256(bytes.fromhex(sys.argv[1])).hexdigest())' "$P")
PR=$(lnc bob addholdinvoice "$H" 1000 | jget payment_request)
(lnc alice payinvoice --force "$PR" > "$W/pay-bob.json" 2>&1 || true) &
state() { [ "$(lnc bob lookupinvoice "$H" | jget state)" = "$1" ]; }
if wait_until 60 state ACCEPTED; then
  result "LND hold invoice: payer's HTLC held (ACCEPTED) without the preimage" ok "hash chosen by the payer"
  lnc bob settleinvoice "$P" >/dev/null
  wait_until 60 state SETTLED && result "LND hold invoice: settled with the payer-released preimage" ok "SETTLED" \
    || result "LND hold invoice: settled with the payer-released preimage" fail "state $(lnc bob lookupinvoice "$H" | jget state)"
else
  result "LND hold invoice: payer's HTLC held (ACCEPTED) without the preimage" fail "state $(lnc bob lookupinvoice "$H" | jget state)"
fi

# --- ldk-node: vector, then a hold invoice on alice's hash over a channel from alice ----------
log "ldk-node"
P2=$(python3 -c 'import os;print(os.urandom(32).hex())')
H2=$(python3 -c 'import hashlib,sys;print(hashlib.sha256(bytes.fromhex(sys.argv[1])).hexdigest())' "$P2")
mkdir -p "$W/dave"
"$LDK" "$W/dave" 127.0.0.1:18443 spike spike 127.0.0.1:9738 "${PK[alice]}" "$H2" "$P2" "$MSG" > "$W/dave/ldk.log" 2>&1 &
LDK_PID=$!
wait_until 120 test -s "$W/dave/node_id" || { log "ldk-node did not start"; tail -30 "$W/dave/ldk.log"; exit 1; }
DAVE=$(cat "$W/dave/node_id")
cat "$W/dave/vector.json" >> "$OUT/vectors.jsonl"; echo >> "$OUT/vectors.jsonl"
lnc alice connect "$DAVE@127.0.0.1:9738" >/dev/null
lnc alice openchannel --node_key "$DAVE" --local_amt 500000 >/dev/null
mine 6
if wait_until 180 test -s "$W/dave/invoice"; then
  (lnc alice payinvoice --force "$(cat "$W/dave/invoice")" > "$W/pay-dave.json" 2>&1 || true) &
  if wait_until 120 test -s "$W/dave/received"; then
    result "ldk-node receive_for_hash/claim_for_hash on a payer-chosen hash" ok "claimable $(cat "$W/dave/claimable") msat, received $(cat "$W/dave/received") msat"
  else
    result "ldk-node receive_for_hash/claim_for_hash on a payer-chosen hash" fail "no PaymentReceived; see ldk.log"
    tail -30 "$W/dave/ldk.log"
  fi
else
  result "ldk-node receive_for_hash/claim_for_hash on a payer-chosen hash" fail "no channel/invoice"
  tail -30 "$W/dave/ldk.log"
fi
wait "$LDK_PID" 2>/dev/null || true

# --- Core Lightning: signing only (best effort: a prebuilt Ubuntu 24.04 binary) ---------------
log "Core Lightning $CLN_V"
if fetch "https://github.com/ElementsProject/lightning/releases/download/$CLN_V/clightning-$CLN_V-Ubuntu-24.04-amd64.tar.xz" "$CLN_SHA" "$W/dl/cln.txz"; then
  mkdir -p "$W/cln-root" "$W/carol"
  tar xJf "$W/dl/cln.txz" -C "$W/cln-root"
  (apt-get install -y -qq libsodium23 libsqlite3-0 libpq5 zlib1g >/dev/null 2>&1 || true)
  LD=$(find "$W/cln-root" -type f -name lightningd -path '*bin*' | head -1)
  CLI=$(find "$W/cln-root" -type f -name lightning-cli | head -1)
  if [ -n "$LD" ] && "$LD" --network=regtest --lightning-dir="$W/carol" --bitcoin-rpcuser=spike \
      --bitcoin-rpcpassword=spike --bitcoin-rpcport=18443 --bitcoin-cli="$W/bin/bitcoin-cli" \
      --bitcoin-datadir="$W/btc" --addr=127.0.0.1:9737 --log-file="$W/carol/log" --daemon 2>"$W/carol/start.err"; then
    lc() { "$CLI" --network=regtest --lightning-dir="$W/carol" "$@"; }
    if wait_until 60 lc getinfo; then
      CPK=$(lc getinfo | jget id)
      emit vectors.jsonl impl "cln-$CLN_V" msg "$MSG" sig "$(lc signmessage "$MSG" | jget zbase)" pubkey "$CPK"
      S=$(stmt "$CPK")
      emit statements.jsonl impl "cln-$CLN_V" statement "$S" sig "$(lc signmessage "$S" | jget zbase)"
      lc listconfigs 2>/dev/null | grep -qi hold && log "CLN: a hold-invoice option is present" || log "CLN: no built-in hold invoices (plugin needed)"
    else
      log "CLN did not answer getinfo"; tail -20 "$W/carol/log" 2>/dev/null || true
    fi
  else
    log "CLN prebuilt binary did not start here (skipped): $(head -c 300 "$W/carol/start.err" 2>/dev/null)"
  fi
fi

# --- verify -----------------------------------------------------------------------------------
log "collected:"
for f in vectors statements nostr_events results; do echo "== $f.jsonl"; cat "$OUT/$f.jsonl" 2>/dev/null || true; done
log "check"
"$CHECK" "$OUT"
