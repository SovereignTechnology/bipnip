#!/usr/bin/env bash
# The web client's Liquidity tab on real nodes, in a throwaway Kata VM with network, as root:
#
#   kata-run --cwd ~/Projects/bipnip --net --timeout 3500 -- bash itest/ln/run-web.sh
#
# Regtest bitcoind; LND for alice and bob (each with a bipnip agent daemon) and for carol (the node
# of a real bipnipd instance); the test relay. bipnip-web is built at BIPNIP_WEB_REV and served by
# bipnip-webhost running as ANOTHER user (bipweb), which reaches alice's agent only through its web
# door (`[web]`, a socket in a directory of its own) and cannot open alice's own socket or read her
# files. Headless Chromium (Playwright, a fresh profile) opens the page on the VM's own address over
# plain HTTP (no secure context, as on a tailnet) and does what `itest/web/liquidity.mjs` says:
# pairing (refused while closed, then the code typed where the agent runs), a lease from bob taken
# from the page and approved at alice's command line, watched to its end, bob's record on his next
# offer, a ring joined from the page, and what must be refused (cross-site, replayed, future-dated,
# the operator's requests, a read-only session, a fourth waiting request, a revoked session). A
# second host on 127.0.0.1 shows the loopback origin too. No CSP violation and no request off the
# page's origin, on either. Screenshots leave the VM base64-encoded between markers.
#
# Every key and credential here is a throwaway regtest value inside the VM.

set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
W=${ITEST_WORK:-/tmp/bipnip-web-itest}
rm -rf "$W"; mkdir -p "$W"/{dl,bin,btc,shots}
chmod 755 "$W"
log() { printf '[itest %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
fail() { log "FAIL: $*"; dump; exit 1; }
jget() { python3 -c 'import json,sys
d=json.load(sys.stdin)
for k in sys.argv[1:]: d=d[k]
print(d)' "$@"; }
wait_until() { local secs=$1; shift; local t=0; until "$@" >/dev/null 2>&1; do sleep 1; t=$((t+1)); [ "$t" -lt "$secs" ] || return 1; done; }
dump() {
  for f in "$W"/daemon-*.err "$W"/daemon-*.out "$W"/host*.log "$W"/hook.log; do
    [ -s "$f" ] && sed "s|^|  $(basename "$f"): |" "$f" | tail -60
  done
  [ -s "$W/instance/mostrod.log" ] && sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$W/instance/mostrod.log" | tail -40 | sed 's/^/  instance: /'
  return 0
}

BTC_V=30.4
BTC_SHA=051a18c595d9a27df23cb7187ff9fb343e90943cb142af751e12787806500c3d
LND_V=v0.21.5-beta
LND_SHA=2127d4794ea9cbd98e762ba16283615158743646d2d6d2a5bf88664dd6255ce7
# bipnipd main, as run-lnd.sh builds it.
BIPNIPD_REV=028099e547a30a929fe0dcee36ef4d15d19a3b43
# bipnip-web: the style nonce, the agent client and the Liquidity tab.
BIPNIP_WEB_REV=${BIPNIP_WEB_REV:-de3bee8a1acf376354884725a43ccab56130c980}
# Playwright: the release W0 ran with.
PW=1.64.1
fetch() { curl -fsSL --retry 3 -o "$3" "$1"; echo "$2  $3" | sha256sum -c --quiet; }

log "tools"
export DEBIAN_FRONTEND=noninteractive
need=()
for t in protoc python3 curl git node npm runuser; do command -v "$t" >/dev/null || need+=("$t"); done
if [ ${#need[@]} -gt 0 ]; then
  apt-get update -qq
  apt-get install -y -qq protobuf-compiler python3 curl ca-certificates git nodejs npm util-linux >/dev/null
fi
log "node $(node --version), npm $(npm --version)"

log "bipnip-web ${BIPNIP_WEB_REV:0:12} (built while the rest builds)"
git clone --quiet https://github.com/SovereignTechnology/bipnip-web "$W/web"
git -C "$W/web" fetch --quiet origin "$BIPNIP_WEB_REV"
git -C "$W/web" checkout --quiet --detach "$BIPNIP_WEB_REV"
[ "$(git -C "$W/web" rev-parse HEAD)" = "$BIPNIP_WEB_REV" ]
(cd "$W/web" && npm ci --ignore-scripts --no-audit --no-fund --loglevel=error && npm run --silent build:bipnip) \
  > "$W/web-build.log" 2>&1 &
WEB_BUILD=$!
mkdir -p "$W/pw"
cp "$REPO/itest/web/liquidity.mjs" "$W/pw/"
(cd "$W/pw" && npm init -y >/dev/null && npm i --no-audit --no-fund --loglevel=error "playwright@$PW" \
  && npx --yes "playwright@$PW" install --with-deps chromium) > "$W/pw-install.log" 2>&1 &
PW_INSTALL=$!

log "downloads (sha256-pinned)"
fetch "https://bitcoincore.org/bin/bitcoin-core-$BTC_V/bitcoin-$BTC_V-x86_64-linux-gnu.tar.gz" "$BTC_SHA" "$W/dl/btc.tgz"
fetch "https://github.com/lightningnetwork/lnd/releases/download/$LND_V/lnd-linux-amd64-$LND_V.tar.gz" "$LND_SHA" "$W/dl/lnd.tgz"
tar xzf "$W/dl/btc.tgz" -C "$W/dl"; cp "$W/dl/bitcoin-$BTC_V/bin/"{bitcoind,bitcoin-cli} "$W/bin/"
tar xzf "$W/dl/lnd.tgz" -C "$W/dl"; cp "$W/dl/lnd-linux-amd64-$LND_V/"{lnd,lncli} "$W/bin/"
export PATH=$W/bin:$PATH

log "build: the agent (LND), the command line, the web host, the test relay and key, the instance"
cargo build --quiet --release --manifest-path "$REPO/Cargo.toml" -p bipnip-agent -p bipnip-cli \
  -p bipnip-webhost --features bipnip-agent/lnd --target-dir "$W/target-ws"
cargo build --quiet --release --manifest-path "$HERE/Cargo.toml" --bin test-relay --bin test-key \
  --target-dir "$W/target"
git clone --quiet https://github.com/SovereignTechnology/bipnipd "$W/bipnipd"
git -C "$W/bipnipd" checkout --quiet "$BIPNIPD_REV"
cargo build --quiet --release --locked --manifest-path "$W/bipnipd/Cargo.toml" --bin mostrod --target-dir "$W/target-d"
AGENT=$W/target-ws/release/bipnip-agent
CLI=$W/target-ws/release/bipnip

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

log "LND: alice, bob (agents) and carol (the instance's node)"
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
lnc() { local n=$1; shift; lncli --lnddir="$W/$n" --network=regtest --rpcserver=127.0.0.1:${RPC[$n]} "$@"; }
synced() { [ "$(lnc "$1" getinfo | jget synced_to_chain)" = "True" ]; }
for n in alice bob carol; do wait_until 120 synced "$n" || { tail -20 "$W/$n/lnd.log"; fail "$n did not sync"; }; done
mac() { echo "$W/$1/data/chain/bitcoin/regtest/admin.macaroon"; }

log "fund alice and bob; a channel alice -> bob to pay over"
for n in alice bob; do $BCLI -rpcwallet=miner sendtoaddress "$(lnc "$n" newaddress p2wkh | jget address)" 5 >/dev/null; done
$BCLI generatetoaddress 6 "$MADDR" >/dev/null
funded() { [ "$(lnc "$1" walletbalance | jget confirmed_balance)" -gt 0 ]; }
wait_until 60 funded alice; wait_until 60 funded bob
BOB_KEY=$(lnc bob getinfo | jget identity_pubkey)
lnc alice connect "$BOB_KEY@127.0.0.1:${P2P[bob]}" >/dev/null
lnc alice openchannel --node_key "$BOB_KEY" --local_amt 2000000 >/dev/null
$BCLI generatetoaddress 6 "$MADDR" >/dev/null
pay_channel() { [ "$(lnc alice listchannels | python3 -c 'import json,sys; print(sum(1 for c in json.load(sys.stdin)["channels"] if c["active"]))')" -ge 1 ]; }
wait_until 90 pay_channel || fail "the channel alice -> bob did not become active"

log "the test relay and the instance"
"$W/target/release/test-relay" 7777 > "$W/relay.log" 2>&1 &
wait_until 30 grep -q listening "$W/relay.log"
read -r INSTANCE_NSEC INSTANCE_PUB < <("$W/target/release/test-key")
mkdir -p "$W/instance"
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
# Its node (carol) has no peer, so its graph knows no seller: run-lnd.sh says more.
text += "\n[liquidity]\nrequire_seller_in_graph = false\n"
open(sys.argv[2], "w").write(text)
PY
chmod 600 "$W/instance/settings.toml"
TERM=xterm "$W/target-d/release/mostrod" --dirsettings "$W/instance" > "$W/instance/mostrod.log" 2>&1 < /dev/null &
wait_until 120 grep -q "Scheduler Started" "$W/instance/mostrod.log" || fail "the instance did not start"

log "the web host's own user, and the web door's directory"
id bipweb >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin bipweb
WEB_UID=$(id -u bipweb)
DOOR_DIR=/run/bipnip-web
rm -rf "$DOOR_DIR"; install -d -m 2750 -o root -g bipweb "$DOOR_DIR"

log "agents: alice (with the web door) and bob"
agent_conf() { # name, policy lines, more
  cat > "$W/$1-d.toml" <<CONF
network = "regtest"
data_dir = "$W/$1-agent"
relays = ["ws://127.0.0.1:7777"]

[lnd]
address = "https://127.0.0.1:${RPC[$1]}"
tls_cert = "$W/$1/tls.cert"
macaroon = "$(mac "$1")"

[[instance]]
pubkey = "$INSTANCE_PUB"

[policy]
$2
$3
CONF
}
# Alice's limits stay at zero: what her page asks for is parked whatever they are.
agent_conf alice "max_runs = 4" "
[web]
socket = \"$DOOR_DIR/web.sock\"
socket_mode = 0o660
allow_uids = [$WEB_UID]"
# Bob offers and proposes from his command line without being asked.
agent_conf bob "max_runs = 4
max_capacity_per_day_sat = 10000000"
for n in alice bob; do "$AGENT" --config "$W/$n-d.toml" init > /dev/null; done
bip() { local n=$1; shift; "$CLI" --socket "$W/$n-agent/agent.sock" "$@"; }
for n in alice bob; do
  "$AGENT" --config "$W/$n-d.toml" run --poll 2 >> "$W/daemon-$n.out" 2>> "$W/daemon-$n.err" &
  wait_until 90 bip "$n" status || fail "$n's daemon did not come up"
done
[ -S "$DOOR_DIR/web.sock" ] || fail "alice's web door was not opened"
[ "$(stat -c '%a %G' "$DOOR_DIR/web.sock")" = "660 bipweb" ] || fail "the web door is $(stat -c '%a %G' "$DOOR_DIR/web.sock")"

log "the boundary: the web host's user reaches the door and nothing else of alice's"
if runuser -u bipweb -- cat "$W/alice-agent/mnemonic" > /dev/null 2>&1; then fail "bipweb read alice's mnemonic"; fi
reach() { runuser -u bipweb -- python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])' "$1"; }
if reach "$W/alice-agent/agent.sock" 2>/dev/null; then fail "bipweb opened alice's own socket"; fi
reach "$DOOR_DIR/web.sock" || fail "bipweb cannot open the web door"

log "the web host, as bipweb, on the VM's address and on loopback"
wait "$WEB_BUILD" || { tail -40 "$W/web-build.log"; fail "bipnip-web did not build"; }
install -d -m 755 /opt/bipnip-web
rm -rf /opt/bipnip-web/dist; cp -r "$W/web/dist" /opt/bipnip-web/dist; chmod -R a+rX /opt/bipnip-web
install -m 755 "$W/target-ws/release/bipnip-webhost" /opt/bipnip-web/bipnip-webhost
IP=$(ip -4 -o addr show scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)
[ -n "$IP" ] || IP=$(hostname -I | awk '{print $1}')
[ -n "$IP" ] || fail "the VM has no address but loopback"
host() { # listen address, log name
  runuser -u bipweb -- /opt/bipnip-web/bipnip-webhost --dist /opt/bipnip-web/dist --listen "$1" \
    --allow-host "$1" --no-tor --agent-socket "$DOOR_DIR/web.sock" > "$W/$2.log" 2>&1 &
}
host "$IP:12600" host-vm
host "127.0.0.1:12601" host-loopback
wait_until 30 grep -q serving "$W/host-vm.log" || fail "the web host did not start"
wait_until 30 grep -q serving "$W/host-loopback.log" || fail "the loopback web host did not start"
log "page at http://$IP:12600 (plain HTTP, not loopback: no secure context)"

log "what the page asks for that happens where the agents run"
cat > "$W/hook.sh" <<HOOK
#!/usr/bin/env bash
# Called by liquidity.mjs. Prints a value on stdout when a step has one; logs to $W/hook.log.
set -euo pipefail
exec 2>> "$W/hook.log"
bip() { local n=\$1; shift; "$CLI" --socket "$W/\$n-agent/agent.sock" "\$@"; }
fail() { echo "hook FAIL: \$*" >&2; echo "hook FAIL: \$*" >> /dev/stderr; exit 1; }
new_order() { # kind (lease|ring), what to look for in the line
  local seen=\$1 t=0
  while [ \$t -lt 120 ]; do
    while read -r order; do
      grep -qx "\$order" "$W/orders-seen" 2>/dev/null || { echo "\$order" >> "$W/orders-seen"; echo "\$order"; return 0; }
    done < <(bip bob "\$seen" list | grep -E "state=(open|seated)" | sed -n 's/.*order=\([0-9a-f-]*\).*/\1/p' | grep -v '^-\$' || true)
    sleep 2; t=\$((t+2))
  done
  fail "bob's \$seen order did not open"
}
case "\$1" in
  pair-open) bip alice pair open --secs 180 > /dev/null ;;
  pair-check)
    bip alice pair > "$W/pair-list.json"
    grep -q '"browser"' "$W/pair-list.json" || fail "the waiting page is not listed: \$(cat "$W/pair-list.json")"
    if grep -q "\${2}" "$W/pair-list.json"; then fail "the pairing code is listed where the agent runs"; fi ;;
  pair-approve) bip alice pair approve "\$2" --scope trade --label browser > /dev/null ;;
  pair-approve-read) bip alice pair approve "\$2" --label reader > /dev/null ;;
  bob-offer)
    bip bob lease offer --min-capacity 300000 --max-capacity 600000 --duration 6 --fee-base 100 \\
      --fee-ppm 1000 --feerate 2 --confs 3 --expires-in 3600 > /dev/null
    new_order lease ;;
  bob-ring)
    bip bob ring create --capacity 250000 --slots 2 --feerate 2 --window 3600 \\
      --node-address 127.0.0.1:${P2P[bob]} > /dev/null
    new_order ring ;;
  approve)
    bip alice approvals > "$W/approvals.json"
    python3 - "$W/approvals.json" "\$2" <<'PY'
import json, sys
listed = json.load(open(sys.argv[1]))["approvals"]
mine = [a for a in listed if a["approval"] == sys.argv[2]]
assert mine, listed
assert mine[0]["asked_by"]["label"] == "browser", mine
assert mine[0]["over"][0].startswith("asked from a paired web page"), mine
PY
    bip alice approve --yes "\$2" > /dev/null ;;
  mine) $BCLI generatetoaddress "\$2" "$MADDR" > /dev/null ;;
  revoke) bip alice revoke "\$2" > /dev/null ;;
  approvals-empty)
    [ "\$(bip alice approvals | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["approvals"]))')" = 0 ] \\
      || fail "the revoked page's requests still wait" ;;
  *) fail "unknown step \$1" ;;
esac
HOOK
chmod +x "$W/hook.sh"

wait "$PW_INSTALL" || { tail -40 "$W/pw-install.log"; fail "Playwright did not install"; }
log "headless Chromium (Playwright $PW): the Liquidity tab"
rc=0
(cd "$W/pw" && node liquidity.mjs "http://$IP:12600" "http://127.0.0.1:12601" "$W/hook.sh" "$W/web" "$W/shots") \
  > "$W/summary.json" 2> "$W/browser.log" || rc=$?
sed 's/^/  /' "$W/browser.log" | tail -80
cat "$W/summary.json"

for png in "$W"/shots/*.png; do
  [ -e "$png" ] || continue
  name=$(basename "$png" .png)
  echo "=== WEB SCREENSHOT $name BASE64 BEGIN"
  base64 -w 0 "$png"; echo
  echo "=== WEB SCREENSHOT $name BASE64 END"
done

[ "$rc" -eq 0 ] || fail "the Liquidity tab run did not pass (status $rc)"
log "WEB E2E PASS: a page served over plain HTTP by a web host of another user, paired by a code typed where the agent runs; a lease and a ring asked for from the page, parked, approved at the command line with the real numbers, followed to their end; the seller's record on the book; cross-site, replayed, future-dated and operator requests, a read-only session, a fourth waiting request and a revoked session refused; no CSP violation and no request off the page's origin"
