#!/usr/bin/env bash
# bipnip-ln conformance for the LND backend, in a throwaway Kata VM with network, as root:
#
#   kata-run --cwd ~/Projects/bipnip --net --timeout 7200 -- bash itest/ln/run-lnd.sh
#
# Starts regtest bitcoind and two LND nodes from sha256-pinned releases, funds the payer, and runs
# `lnd-conformance`, which calls every LightningBackend method of LndBackend the way a lease does.
# Every key and credential here is a throwaway regtest value inside the VM.

set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
W=${ITEST_WORK:-/tmp/bipnip-ln-itest}
rm -rf "$W"; mkdir -p "$W"/{dl,bin,btc}
log() { printf '[itest %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
jget() { python3 -c 'import json,sys
d=json.load(sys.stdin)
for k in sys.argv[1:]: d=d[k]
print(d)' "$@"; }
wait_until() { local secs=$1; shift; local t=0; until "$@" >/dev/null 2>&1; do sleep 1; t=$((t+1)); [ "$t" -lt "$secs" ] || return 1; done; }

BTC_V=30.4
BTC_SHA=051a18c595d9a27df23cb7187ff9fb343e90943cb142af751e12787806500c3d
LND_V=v0.21.5-beta
LND_SHA=2127d4794ea9cbd98e762ba16283615158743646d2d6d2a5bf88664dd6255ce7
fetch() { curl -fsSL --retry 3 -o "$3" "$1"; echo "$2  $3" | sha256sum -c --quiet; }

log "tools"
export DEBIAN_FRONTEND=noninteractive
if ! command -v protoc >/dev/null || ! command -v python3 >/dev/null || ! command -v curl >/dev/null; then
  apt-get update -qq
  apt-get install -y -qq protobuf-compiler python3 curl ca-certificates >/dev/null
fi

log "downloads (sha256-pinned)"
fetch "https://bitcoincore.org/bin/bitcoin-core-$BTC_V/bitcoin-$BTC_V-x86_64-linux-gnu.tar.gz" "$BTC_SHA" "$W/dl/btc.tgz"
fetch "https://github.com/lightningnetwork/lnd/releases/download/$LND_V/lnd-linux-amd64-$LND_V.tar.gz" "$LND_SHA" "$W/dl/lnd.tgz"
tar xzf "$W/dl/btc.tgz" -C "$W/dl"; cp "$W/dl/bitcoin-$BTC_V/bin/"{bitcoind,bitcoin-cli} "$W/bin/"
tar xzf "$W/dl/lnd.tgz" -C "$W/dl"; cp "$W/dl/lnd-linux-amd64-$LND_V/"{lnd,lncli} "$W/bin/"
export PATH=$W/bin:$PATH

log "unit tests and clippy of bipnip-ln with the lnd feature"
cargo clippy --quiet --manifest-path "$HERE/../../Cargo.toml" -p bipnip-ln --features lnd --all-targets --target-dir "$W/target-ws" -- -D warnings
cargo test --manifest-path "$HERE/../../Cargo.toml" -p bipnip-ln --features lnd --target-dir "$W/target-ws" > "$W/unit.log" 2>&1 \
  || { tail -40 "$W/unit.log"; exit 1; }
grep -E '^test result' "$W/unit.log"

log "build the conformance program"
cargo clippy --quiet --manifest-path "$HERE/Cargo.toml" --target-dir "$W/target" -- -D warnings
cargo build --quiet --release --manifest-path "$HERE/Cargo.toml" --target-dir "$W/target"

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
for n in alice bob; do wait_until 120 synced "$n" || { log "$n did not sync"; tail -20 "$W/$n/lnd.log"; exit 1; }; done

log "fund the payer"
$BCLI -rpcwallet=miner sendtoaddress "$(lnc alice newaddress p2wkh | jget address)" 5 >/dev/null
$BCLI generatetoaddress 6 "$MADDR" >/dev/null
funded() { [ "$(lnc alice walletbalance | jget confirmed_balance)" -gt 0 ]; }
wait_until 60 funded

log "conformance"
printf '#!/usr/bin/env bash\nexec %s generatetoaddress "$1" %s >/dev/null\n' "$BCLI" "$MADDR" > "$W/mine.sh"; chmod +x "$W/mine.sh"
pair() { python3 -c 'import os,hashlib; p=os.urandom(32); print(p.hex()+":"+hashlib.sha256(p).hexdigest())'; }
mac() { echo "$W/$1/data/chain/bitcoin/regtest/admin.macaroon"; }
A_ADDR=https://127.0.0.1:${RPC[alice]} A_CERT=$W/alice/tls.cert A_MAC=$(mac alice) \
B_ADDR=https://127.0.0.1:${RPC[bob]} B_CERT=$W/bob/tls.cert B_MAC=$(mac bob) \
B_P2P=127.0.0.1:${P2P[bob]} HASHES="$(pair),$(pair),$(pair)" \
MINE="$W/mine.sh" \
  "$W/target/release/lnd-conformance"

# --- a whole lease between the two nodes ------------------------------------------------------
log "fund the seller (bob) on chain"
$BCLI -rpcwallet=miner sendtoaddress "$(lnc bob newaddress p2wkh | jget address)" 5 >/dev/null
$BCLI generatetoaddress 6 "$MADDR" >/dev/null
bob_funded() { [ "$(lnc bob walletbalance | jget confirmed_balance)" -gt 0 ]; }
wait_until 60 bob_funded
log "lease end to end"
A_ADDR=https://127.0.0.1:${RPC[alice]} A_CERT=$W/alice/tls.cert A_MAC=$(mac alice) \
B_ADDR=https://127.0.0.1:${RPC[bob]} B_CERT=$W/bob/tls.cert B_MAC=$(mac bob) \
A_P2P=127.0.0.1:${P2P[alice]} MINE="$W/mine.sh" \
  "$W/target/release/lease-e2e"

# --- the agent, end to end: init, bind + publish, verify from a second agent, revoke -----------
log "build bipnip-agent with LND support"
cargo build --quiet --release --manifest-path "$HERE/../../Cargo.toml" -p bipnip-agent --features lnd --target-dir "$W/target-ws"
AGENT=$W/target-ws/release/bipnip-agent
"$W/target/release/test-relay" 7777 > "$W/relay.log" 2>&1 &
wait_until 30 grep -q listening "$W/relay.log"

agent_conf() { # name
  cat > "$W/$1.toml" <<CONF
network = "regtest"
data_dir = "$W/$1-agent"
relays = ["ws://127.0.0.1:7777"]

[lnd]
address = "https://127.0.0.1:${RPC[$1]}"
tls_cert = "$W/$1/tls.cert"
macaroon = "$(mac "$1")"
CONF
}
agent_conf alice; agent_conf bob
fail() { log "FAIL: $*"; exit 1; }

log "agent: init"
"$AGENT" --config "$W/alice.toml" init > "$W/init.out"
"$AGENT" --config "$W/bob.toml" init > /dev/null
[ "$(stat -c %a "$W/alice-agent/mnemonic")" = 600 ] || fail "mnemonic file is not 600"
[ "$(stat -c %a "$W/alice-agent")" = 700 ] || fail "data dir is not 700"
[ "$(wc -w < "$W/alice-agent/mnemonic")" = 12 ] || fail "mnemonic is not 12 words"
grep -qF -- "$(cut -d' ' -f1-3 "$W/alice-agent/mnemonic")" "$W/init.out" && fail "init printed mnemonic words"
"$AGENT" --config "$W/alice.toml" init > /dev/null 2>&1 && fail "init overwrote an existing mnemonic"

log "agent: a mnemonic file others can read is refused"
chmod 644 "$W/alice-agent/mnemonic"
"$AGENT" --config "$W/alice.toml" whoami > /dev/null 2> "$W/open.err" && fail "agent read a 644 mnemonic"
grep -q "must be 600" "$W/open.err" || fail "no clear error for the open mnemonic file"
chmod 600 "$W/alice-agent/mnemonic"

log "agent: whoami"
"$AGENT" --config "$W/alice.toml" whoami | sed 's/^/  /'
APK=$(lnc alice getinfo | jget identity_pubkey)
"$AGENT" --config "$W/alice.toml" whoami | grep -q "$APK" || fail "whoami does not show the node"

log "agent: bind + publish, then verify from the other agent"
"$AGENT" --config "$W/bob.toml" verify-binding "$APK" > /dev/null 2>&1 && fail "verified a binding before any was published"
"$AGENT" --config "$W/alice.toml" bind --publish > "$W/bind.json" 2> "$W/bind.err" || { cat "$W/bind.err"; fail "bind"; }
grep -q "accepted by" "$W/bind.err" || fail "no relay accepted the binding"
"$AGENT" --config "$W/bob.toml" verify-binding "$APK" | tee "$W/verify.out" | sed 's/^/  /'
grep -q "^bound: node $APK" "$W/verify.out" || fail "the other agent did not verify the binding"
python3 - "$W/bind.json" "$APK" <<'PY'
import json, sys
e = json.load(open(sys.argv[1])); tags = {t[0]: t[1] for t in e["tags"]}
assert e["kind"] == 31735 and tags["d"] == sys.argv[2] == tags["node"], "event shape"
assert tags["statement"].startswith("nostr-node-binding:v1:bind:regtest:" + e["pubkey"] + ":" + sys.argv[2] + ":"), "statement"
assert len(tags["node_sig"]) == 104, "zbase32 signature length"
PY

log "agent: revoke + publish; the other agent now refuses"
sleep 2
"$AGENT" --config "$W/alice.toml" bind --revoke --publish > /dev/null 2>&1 || fail "revoke"
"$AGENT" --config "$W/bob.toml" verify-binding "$APK" > "$W/verify2.out" 2>&1 && fail "a revoked binding still verified"
grep -q "revoked" "$W/verify2.out" || fail "no revocation reported"

log "AGENT E2E PASS: init, bind, publish, verify, revoke against a real LND node"

# --- two agents lease through a real instance ---------------------------------------------------
# bipnipd main (lease handlers, the public lease order, ring handlers) plus ring bonds: the head of
# bipnipd #7, which is still in review. Move this to main once that is merged.
BIPNIPD_REV=56c5107b5f8d6c8ead78ce9ccd699d443ffb74d8
log "build the instance (bipnipd ${BIPNIPD_REV:0:12})"
git clone --quiet https://github.com/SovereignTechnology/bipnipd "$W/bipnipd"
git -C "$W/bipnipd" checkout --quiet "$BIPNIPD_REV"
cargo build --quiet --release --locked --manifest-path "$W/bipnipd/Cargo.toml" --bin mostrod --target-dir "$W/target-d"

log "a third node (carol) for the instance"
RPC[carol]=10011; REST[carol]=8083; P2P[carol]=9737
mkdir -p "$W/carol"
lnd --lnddir="$W/carol" --bitcoin.regtest --bitcoin.node=bitcoind \
  --bitcoind.rpchost=127.0.0.1:18443 --bitcoind.rpcuser=itest --bitcoind.rpcpass=itest \
  --bitcoind.zmqpubrawblock=tcp://127.0.0.1:28332 --bitcoind.zmqpubrawtx=tcp://127.0.0.1:28333 \
  --noseedbackup --rpclisten=127.0.0.1:${RPC[carol]} --restlisten=127.0.0.1:${REST[carol]} \
  --listen=127.0.0.1:${P2P[carol]} --alias=carol --trickledelay=1000 > "$W/carol/lnd.log" 2>&1 &
wait_until 120 synced carol || { log "carol did not sync"; tail -20 "$W/carol/lnd.log"; exit 1; }

log "instance: settings and start"
read -r INSTANCE_NSEC INSTANCE_PUB < <("$W/target/release/test-key")
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
text += "\n[liquidity]\nbonds = true\nmax_bond_sat = 100000\n"
open(sys.argv[2], "w").write(text)
PY
chmod 600 "$W/instance/settings.toml"
TERM=xterm "$W/target-d/release/mostrod" --dirsettings "$W/instance" > "$W/instance/mostrod.log" 2>&1 < /dev/null &
INSTANCE_PID=$!
instance_log() { sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$W/instance/mostrod.log" | tail -"${1:-40}" | sed 's/^/  instance: /'; }
wait_until 120 grep -q "Scheduler Started" "$W/instance/mostrod.log" \
  || { instance_log; fail "the instance did not start"; }

for n in alice bob; do printf '\n[instance]\npubkey = "%s"\n' "$INSTANCE_PUB" >> "$W/$n.toml"; done
lease_dump() {
  for f in offer.out offer.err take.out take.err; do [ -s "$W/$f" ] && sed "s/^/  $f: /" "$W/$f"; done
  instance_log 60
}

log "lease: bob offers liquidity on the instance"
"$AGENT" --config "$W/bob.toml" lease offer --min-capacity 500000 --max-capacity 1000000 \
  --duration 144 --fee-base 1000 --fee-ppm 2000 --feerate 2 --confs 3 --expires-in 3600 --poll 2 \
  > "$W/offer.out" 2> "$W/offer.err" &
OFFER_PID=$!
wait_until 90 grep -q "state=open" "$W/offer.out" || { lease_dump; fail "the offer did not open"; }
ORDER=$(grep -m1 "state=open" "$W/offer.out" | sed 's/.*order=\([^ ]*\).*/\1/')
log "lease: order $ORDER is open; alice takes 600000 sat of it"
"$AGENT" --config "$W/alice.toml" lease take "$ORDER" --capacity 600000 \
  --node-address "127.0.0.1:${P2P[alice]}" --poll 2 > "$W/take.out" 2> "$W/take.err" &
TAKE_PID=$!

both_active() { grep -q "state=active" "$W/offer.out" && grep -q "state=active" "$W/take.out"; }
t=0
until both_active; do
  kill -0 "$OFFER_PID" 2>/dev/null || grep -q "state=active" "$W/offer.out" || { lease_dump; fail "the seller's agent stopped early"; }
  kill -0 "$TAKE_PID" 2>/dev/null || grep -q "state=active" "$W/take.out" || { lease_dump; fail "the buyer's agent stopped early"; }
  "$W/mine.sh" 1; sleep 2; t=$((t+1))
  [ "$t" -lt 90 ] || { lease_dump; fail "the lease did not become active"; }
done
wait "$OFFER_PID" || fail "the seller's agent exited with an error"
wait "$TAKE_PID" || fail "the buyer's agent exited with an error"
sed 's/^/  seller: /' "$W/offer.out"; sed 's/^/  buyer:  /' "$W/take.out"

log "lease: what each side holds afterwards"
BPK=$(lnc bob getinfo | jget identity_pubkey)
lnc alice listchannels | python3 -c '
import json, sys
bob = sys.argv[1]
leased = [c for c in json.load(sys.stdin)["channels"]
          if c["remote_pubkey"] == bob and c["capacity"] == "600000" and not c["initiator"]]
assert len(leased) == 1, f"expected one leased 600000 sat channel from bob, found {len(leased)}"
assert leased[0]["active"], "the leased channel is not active"
' "$BPK" || fail "alice does not have the leased channel"
"$AGENT" --config "$W/alice.toml" lease list | grep -q "^buyer order=$ORDER state=active" || fail "the buyer's stored lease is not active"
"$AGENT" --config "$W/bob.toml" lease list | grep -q "^seller order=$ORDER state=active" || fail "the seller's stored lease is not active"
LEASE_FILE=$(ls "$W"/alice-agent/leases/buyer-*.lease)
[ "$(stat -c %a "$LEASE_FILE")" = 600 ] || fail "the buyer's lease file is not 600"
[ "$(stat -c %a "$W/alice-agent/leases")" = 700 ] || fail "the leases directory is not 700"

log "lease: the instance coordinated it and never held the preimage"
python3 - "$W/instance/mostro.db" "$ORDER" <<'PY' || fail "the instance's record of the lease is wrong"
import sqlite3, sys
db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
status = db.execute("SELECT status FROM lease_offers WHERE id = ?", (sys.argv[2],)).fetchone()
assert status == ("active",), status
txid, = db.execute("SELECT funding_txid FROM leases WHERE offer_id = ?", (sys.argv[2],)).fetchone()
assert txid and len(txid) == 64, txid
PY
# The preimage is read into a variable and only ever searched for, never printed.
PREIMAGE=$(sed -n 3p "$LEASE_FILE")
[ "${#PREIMAGE}" = 64 ] || fail "the lease file has no preimage line"
grep -qF -- "$PREIMAGE" "$LEASE_FILE" || fail "the search for the preimage does not find it where it is"
for f in "$W"/instance/mostro.db* "$W/instance/mostrod.log" "$W/relay.log" \
         "$W"/alice-agent/leases/*.json "$W"/bob-agent/leases/*.json "$W"/*.out "$W"/*.err; do
  [ -e "$f" ] || fail "nothing to search at $f"
  if grep -aqF -- "$PREIMAGE" "$f"; then fail "the preimage is in $f"; fi
done

log "lease: a second lease, half the fee at the end, six blocks long"
"$AGENT" --config "$W/bob.toml" lease offer --min-capacity 500000 --max-capacity 1000000 \
  --duration 6 --upfront 50 --fee-base 1000 --fee-ppm 2000 --feerate 2 --confs 3 --expires-in 3600 --poll 2 \
  > "$W/offer2.out" 2> "$W/offer2.err" &
OFFER_PID=$!
lease2_dump() {
  for f in offer2.out offer2.err take2.out take2.err; do [ -s "$W/$f" ] && sed "s/^/  $f: /" "$W/$f"; done
  instance_log 60
}
wait_until 90 grep -q "state=open" "$W/offer2.out" || { lease2_dump; fail "the second offer did not open"; }
ORDER2=$(grep -m1 "state=open" "$W/offer2.out" | sed 's/.*order=\([^ ]*\).*/\1/')
"$AGENT" --config "$W/alice.toml" lease take "$ORDER2" --capacity 777000 \
  --node-address "127.0.0.1:${P2P[alice]}" --poll 2 > "$W/take2.out" 2> "$W/take2.err" &
TAKE_PID=$!
both_complete() { grep -q "state=complete" "$W/offer2.out" && grep -q "state=complete" "$W/take2.out"; }
t=0
until both_complete; do
  kill -0 "$OFFER_PID" 2>/dev/null || grep -q "state=complete" "$W/offer2.out" || { lease2_dump; fail "the seller's agent stopped early"; }
  kill -0 "$TAKE_PID" 2>/dev/null || grep -q "state=complete" "$W/take2.out" || { lease2_dump; fail "the buyer's agent stopped early"; }
  "$W/mine.sh" 1; sleep 2; t=$((t+1))
  [ "$t" -lt 90 ] || { lease2_dump; fail "the second lease did not complete"; }
done
wait "$OFFER_PID" || fail "the seller's agent exited with an error"
wait "$TAKE_PID" || fail "the buyer's agent exited with an error"
sed 's/^/  seller: /' "$W/offer2.out"; sed 's/^/  buyer:  /' "$W/take2.out"
grep -q "state=completing" "$W/offer2.out" || fail "the seller never billed the completion share"
python3 - "$W/instance/mostro.db" "$ORDER2" <<'PY' || fail "the instance's record of the second lease is wrong"
import sqlite3, sys
db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
status = db.execute("SELECT status FROM lease_offers WHERE id = ?", (sys.argv[2],)).fetchone()
assert status == ("completion",), status
PY
# Both shares reached bob: 1000 + 777000 * 2000 / 1000000 = 2554 sat, 1277 upfront and 1277 at the end.
lnc bob listinvoices --max_invoices 1000 | python3 -c '
import json, sys
settled = sorted(int(i["amt_paid_sat"]) for i in json.load(sys.stdin)["invoices"] if i["state"] == "SETTLED")
assert settled.count(1277) == 2, f"expected two settled 1277 sat invoices at bob, got {settled}"
' || fail "bob was not paid both shares of the second lease"
log "INSTANCE LEASE E2E PASS: two leases through a real bipnipd, two agents and three LND nodes: offer, take, pay, open, verify, release, settle, and the completion share at the end of the lease"

# --- three agents make a ring through the instance ---------------------------------------------
log "ring: a third agent (carol), with funds to open a channel"
agent_conf carol
printf '\n[instance]\npubkey = "%s"\n' "$INSTANCE_PUB" >> "$W/carol.toml"
"$AGENT" --config "$W/carol.toml" init > /dev/null
$BCLI -rpcwallet=miner sendtoaddress "$(lnc carol newaddress p2wkh | jget address)" 1 >/dev/null
$BCLI generatetoaddress 6 "$MADDR" >/dev/null
carol_funded() { [ "$(lnc carol walletbalance | jget confirmed_balance)" -gt 0 ]; }
wait_until 60 carol_funded
ring_dump() {
  for n in bob alice carol; do
    for f in "ring-$n.out" "ring-$n.err"; do [ -s "$W/$f" ] && sed "s/^/  $f: /" "$W/$f"; done
  done
  instance_log 80
}

log "ring: bob proposes a ring of three 314159 sat channels; alice and carol join"
"$AGENT" --config "$W/bob.toml" ring create --capacity 314159 --slots 3 --feerate 2 \
  --window 3600 --expires-in 3600 --node-address "127.0.0.1:${P2P[bob]}" --poll 2 \
  > "$W/ring-bob.out" 2> "$W/ring-bob.err" &
RING_PIDS=($!)
wait_until 90 grep -q "state=seated" "$W/ring-bob.out" || { ring_dump; fail "the ring was not stored by the instance"; }
RING=$(grep -m1 "state=seated" "$W/ring-bob.out" | sed 's/.*order=\([^ ]*\).*/\1/')
for n in alice carol; do
  "$AGENT" --config "$W/$n.toml" ring join "$RING" --feerate 2 \
    --node-address "127.0.0.1:${P2P[$n]}" --poll 2 > "$W/ring-$n.out" 2> "$W/ring-$n.err" &
  RING_PIDS+=($!)
  wait_until 90 grep -q "state=seated\|state=committed\|state=open" "$W/ring-$n.out" \
    || { ring_dump; fail "$n did not get a slot"; }
done
ring_done() { for n in bob alice carol; do grep -q "state=success" "$W/ring-$n.out" || return 1; done; }
t=0
until ring_done; do
  for n in bob alice carol; do
    grep -q "state=partial\|state=canceled" "$W/ring-$n.out" && { ring_dump; fail "the ring ended early for $n"; }
  done
  "$W/mine.sh" 1; sleep 2; t=$((t+1))
  [ "$t" -lt 90 ] || { ring_dump; fail "the ring did not complete"; }
done
for pid in "${RING_PIDS[@]}"; do wait "$pid" || fail "a ring agent exited with an error"; done
for n in bob alice carol; do tail -1 "$W/ring-$n.out" | sed "s/^/  $n: /"; done

log "ring: every node opened one 314159 sat channel and received one, and the instance agrees"
for n in bob alice carol; do
  lnc "$n" listchannels | python3 -c '
import json, sys
ring = [c for c in json.load(sys.stdin)["channels"] if c["capacity"] == "314159"]
opened = [c for c in ring if c["initiator"]]
received = [c for c in ring if not c["initiator"]]
assert len(opened) == 1 and len(received) == 1, f"opened {len(opened)}, received {len(received)}"
assert opened[0]["remote_pubkey"] != received[0]["remote_pubkey"], "a ring of three has two different neighbours"
assert all(c["active"] for c in ring), "a ring channel is not active"
' || fail "$n does not have one ring channel out and one in"
  "$AGENT" --config "$W/$n.toml" ring list | grep -q "^ring order=$RING state=success" || fail "$n's stored ring is not a success"
done
python3 - "$W/instance/mostro.db" "$RING" <<'PY' || fail "the instance's record of the ring is wrong"
import sqlite3, sys
db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
status, taken = db.execute("SELECT status, taken FROM ring_offers WHERE id = ?", (sys.argv[2],)).fetchone()
assert (status, taken) == ("success", 3), (status, taken)
rows = db.execute("SELECT position, commit_sig, edge_txid, edge_verified FROM ring_members WHERE ring_offer_id = ? ORDER BY position", (sys.argv[2],)).fetchall()
assert [r[0] for r in rows] == [0, 1, 2], rows
assert all(r[1] and r[2] and r[3] == 1 for r in rows), rows
PY

log "INSTANCE RING E2E PASS: a ring of three through a real bipnipd: slots, roster, commits, three channels opened and confirmed"

# --- rings with bonds: the instance's node (carol) holds each member's bond ----------------------
BOND=20000
PAYOUT=19800   # the bond less the 1% routing reserve
bond_dump() {
  for f in "$W"/bond-*.out "$W"/bond-*.err; do [ -s "$f" ] && sed "s|^|  $(basename "$f"): |" "$f"; done
  instance_log 120
}
log "bonds: the instance published its node binding; alice and bob may lock up to 50000 sat"
wait_until 60 grep -q "node binding is published" "$W/instance/mostrod.log" \
  || { instance_log; fail "the instance did not publish its node binding"; }
for n in alice bob; do printf '\n[policy]\nmax_bond_sat = 50000\n' >> "$W/$n.toml"; done
# A bond travels over the ring channels just opened: every node must know all three.
"$W/mine.sh" 6
ring_channels_known() {
  for n in alice bob carol; do
    [ "$(lnc "$n" describegraph | python3 -c 'import json,sys; print(sum(1 for e in json.load(sys.stdin)["edges"] if e["capacity"] == "314159"))')" = 3 ] || return 1
  done
}
wait_until 180 ring_channels_known || { bond_dump; fail "the ring channels did not reach every node's graph"; }
order_of() { grep -m1 "state=seated" "$1" | sed 's/.*order=\([^ ]*\).*/\1/'; }
# value state pairs of a node's invoices or payments of one amount, sorted
invoices_of() { lnc "$1" listinvoices | python3 -c 'import json,sys; print(sorted(i["state"] for i in json.load(sys.stdin)["invoices"] if i["value"] == sys.argv[1]))' "$2"; }
payments_of() { lnc "$1" listpayments --include_incomplete | python3 -c 'import json,sys; print(sorted(p["status"] for p in json.load(sys.stdin)["payments"] if p["value_sat"] == sys.argv[1]))' "$2"; }

log "bonded ring: alice proposes two 271828 sat channels with a $BOND sat bond"
"$AGENT" --config "$W/alice.toml" ring create --capacity 271828 --slots 2 --feerate 2 --bond "$BOND" \
  --window 900 --expires-in 3600 --node-address "127.0.0.1:${P2P[alice]}" --poll 2 \
  > "$W/bond-a-alice.out" 2> "$W/bond-a-alice.err" &
BOND_PIDS=($!)
wait_until 90 grep -q "state=seated" "$W/bond-a-alice.out" || { bond_dump; fail "the bonded ring was not stored by the instance"; }
RING_A=$(order_of "$W/bond-a-alice.out")
log "bonded ring: carol's agent has no bond limit and refuses to join; bob joins"
if "$AGENT" --config "$W/carol.toml" ring join "$RING_A" --feerate 2 > "$W/bond-a-carol.out" 2> "$W/bond-a-carol.err"; then
  bond_dump; fail "an agent with no bond limit joined a ring with a bond"
fi
cat "$W/bond-a-carol.out" "$W/bond-a-carol.err" | grep -q "locks at most 0" || { bond_dump; fail "carol's refusal does not name its limit"; }
"$AGENT" --config "$W/bob.toml" ring join "$RING_A" --feerate 2 \
  --node-address "127.0.0.1:${P2P[bob]}" --poll 2 > "$W/bond-a-bob.out" 2> "$W/bond-a-bob.err" &
BOND_PIDS+=($!)
bonded_done() { for n in alice bob; do grep -q "state=success" "$W/bond-a-$n.out" || return 1; done; }
t=0
until bonded_done; do
  for n in alice bob; do
    grep -q "state=partial\|state=canceled" "$W/bond-a-$n.out" && { bond_dump; fail "the bonded ring ended early for $n"; }
  done
  "$W/mine.sh" 1; sleep 2; t=$((t+1))
  [ "$t" -lt 120 ] || { bond_dump; fail "the bonded ring did not complete"; }
done
for pid in "${BOND_PIDS[@]}"; do wait "$pid" || fail "a ring agent exited with an error"; done
log "bonded ring: both bonds were locked before anyone opened, and both came back"
for n in alice bob; do
  # The first line that shows a channel of ours comes after the line that shows our bond paid.
  python3 - "$W/bond-a-$n.out" <<'PY' || { bond_dump; fail "$n opened before its bond was paid"; }
import sys
lines = open(sys.argv[1]).read().splitlines()
paid = next(i for i, line in enumerate(lines) if " bond=sent " in line)
opened = next(i for i, line in enumerate(lines) if " opened=" in line and " opened=- " not in line)
assert paid < opened, (paid, opened)
PY
done
python3 - "$W/instance/mostro.db" "$RING_A" "$BOND" <<'PY' || { bond_dump; fail "the instance's record of the returned bonds is wrong"; }
import sqlite3, sys
db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
assert db.execute("SELECT status FROM ring_offers WHERE id = ?", (sys.argv[2],)).fetchone() == ("success",)
rows = db.execute("SELECT state, node_done, amount_sat FROM liquidity_bonds WHERE kind = 'ring' AND order_id = ?", (sys.argv[2],)).fetchall()
assert sorted(rows) == [("released", 1, int(sys.argv[3]))] * 2, rows
PY
bonds_back() {
  [ "$(invoices_of carol "$BOND")" = "['CANCELED', 'CANCELED']" ] || return 1
  for n in alice bob; do [ "$(payments_of "$n" "$BOND")" = "['FAILED']" ] || return 1; done
}
wait_until 60 bonds_back || { invoices_of carol "$BOND"; payments_of alice "$BOND"; payments_of bob "$BOND"; bond_dump; fail "a bond did not come back"; }

log "bonded ring: bob proposes another (161803 sat, 180 s to open); alice joins; bob's agent stops once its bond is paid"
"$AGENT" --config "$W/bob.toml" ring create --capacity 161803 --slots 2 --feerate 2 --bond "$BOND" \
  --window 180 --expires-in 3600 --node-address "127.0.0.1:${P2P[bob]}" --poll 2 \
  > "$W/bond-b-bob.out" 2> "$W/bond-b-bob.err" &
BOB_PID=$!
wait_until 90 grep -q "state=seated" "$W/bond-b-bob.out" || { bond_dump; fail "the second bonded ring was not stored by the instance"; }
RING_B=$(order_of "$W/bond-b-bob.out")
"$AGENT" --config "$W/alice.toml" ring join "$RING_B" --feerate 2 \
  --node-address "127.0.0.1:${P2P[alice]}" --poll 2 > "$W/bond-b-alice.out" 2> "$W/bond-b-alice.err" &
ALICE_PID=$!
t=0
until grep -q " bond=sent " "$W/bond-b-bob.out"; do
  sleep 0.2; t=$((t+1))
  [ "$t" -lt 600 ] || { bond_dump; fail "bob never paid its bond"; }
done
kill "$BOB_PID"; wait "$BOB_PID" 2>/dev/null || true
grep -q "state=open" "$W/bond-b-bob.out" && { bond_dump; fail "bob's agent saw the ring open before it was stopped"; }
# Alice opens; her channel needs six confirmations to be announced. No more blocks than that: the
# bonds are payments held against a block height.
t=0
until grep -q " owed-bond=paid " "$W/bond-b-alice.out"; do
  grep -q "state=success\|state=canceled" "$W/bond-b-alice.out" && { bond_dump; fail "the ring did not end with a channel missing"; }
  if grep -q " opened=[0-9a-f]" "$W/bond-b-alice.out" && [ "${MINED:-0}" -lt 8 ]; then "$W/mine.sh" 1; MINED=$(( ${MINED:-0} + 1 )); fi
  sleep 2; t=$((t+1))
  [ "$t" -lt 240 ] || { bond_dump; fail "alice was not paid the bond bob lost"; }
done
wait "$ALICE_PID" || fail "alice's agent exited with an error"
tail -1 "$W/bond-b-alice.out" | sed 's/^/  alice: /'

log "bonded ring: alice's channel stands and her bond came back; bob's bond was paid on to her"
"$AGENT" --config "$W/alice.toml" ring list | grep "^ring order=$RING_B state=partial " | grep -q " owed-bond=paid " \
  || fail "alice's stored ring does not show the bond paid"
lnc alice listchannels | python3 -c '
import json, sys
ring = [c for c in json.load(sys.stdin)["channels"] if c["capacity"] == "161803"]
assert len(ring) == 1 and ring[0]["initiator"] and ring[0]["active"], ring
' || fail "alice does not have exactly the one channel she opened"
python3 - "$W/instance/mostro.db" "$RING_B" <<'PY' || { bond_dump; fail "the instance's record of the taken bond is wrong"; }
import sqlite3, sys
db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
assert db.execute("SELECT status FROM ring_offers WHERE id = ?", (sys.argv[2],)).fetchone() == ("partial",)
rows = db.execute("SELECT state, node_done, payer_identity, recipient_identity FROM liquidity_bonds WHERE kind = 'ring' AND order_id = ? ORDER BY state", (sys.argv[2],)).fetchall()
assert [r[0] for r in rows] == ["paid", "released"] and all(r[1] == 1 for r in rows), rows
# The bond that was taken is owed to the other member, the one that kept its own.
assert rows[0][3] == rows[1][2] and rows[1][3] is None, rows
PY
# On the nodes: alice's bond payment failed (it came back) and bob's succeeded (it was taken);
# alice was paid the payout; the instance's node paid it once and kept no more than the reserve.
[ "$(payments_of alice "$BOND")" = "['FAILED', 'FAILED']" ] || { payments_of alice "$BOND"; fail "alice's second bond did not come back"; }
[ "$(payments_of bob "$BOND")" = "['FAILED', 'SUCCEEDED']" ] || { payments_of bob "$BOND"; fail "bob's second bond was not taken"; }
[ "$(invoices_of alice "$PAYOUT")" = "['SETTLED']" ] || { invoices_of alice "$PAYOUT"; fail "alice was not paid exactly the payout"; }
[ "$(invoices_of carol "$BOND")" = "['CANCELED', 'CANCELED', 'CANCELED', 'SETTLED']" ] || { invoices_of carol "$BOND"; fail "the instance's node did not settle exactly one bond"; }
lnc carol listpayments --include_incomplete | python3 -c '
import json, sys
bond, payout = int(sys.argv[1]), int(sys.argv[2])
paid = [p for p in json.load(sys.stdin)["payments"] if p["status"] == "SUCCEEDED"]
assert len(paid) == 1 and int(paid[0]["value_sat"]) == payout, paid
fee = int(paid[0]["fee_sat"])
assert 0 <= fee <= bond - payout, fee
print(f"  the instance took {bond}, paid on {payout} plus a routing fee of {fee}, and kept {bond - payout - fee}")
' "$BOND" "$PAYOUT" || fail "the instance's node paid out something other than the one payout"
kill "$INSTANCE_PID"

log "INSTANCE BOND E2E PASS: two rings with bonds through a real bipnipd: bonds locked before anyone opened and returned when both opened; with one member gone after paying, the other's channel stood, its bond came back, and the missing member's bond was taken and paid on to it"
