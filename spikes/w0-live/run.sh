#!/usr/bin/env bash
# W0 live check, entirely inside a throwaway Kata VM with network (kata-run --net), as root:
#
#   kata-run --cwd ~/Projects/bipnip --net --timeout 3000 -- bash spikes/w0-live/run.sh
#
# 1. builds bipnip-web at a pinned commit (npm ci --ignore-scripts, npm run build:bipnip) and
#    bipnip-webhost from this checkout;
# 2. starts the host with a fresh Tor client; coordinators come from the UI's federation.json;
# 3. asks each mainnet/testnet coordinator for its public info and order book through the host
#    (read only: no robot, no order, no trade);
# 4. opens http://127.0.0.1:12596/ in headless Chromium (Playwright), clicks Offers, waits for the book,
#    records CSP violations, off-origin requests, failed requests and page errors;
# 5. prints the summary and the screenshot (base64, between markers) for the laptop to save.
# Nothing here runs on the laptop.

set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
W=${W0_WORK:-/tmp/bipnip-w0}
WEB_REV=4b7276875c9bd000116d748165d466526a1d4833   # bipnip-web main after #1
PW=1.64.1                                          # Playwright, newest release at least 7 days old
PORT=12596
rm -rf "$W"; mkdir -p "$W"
log() { printf '[w0 %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }

log "tools"
export DEBIAN_FRONTEND=noninteractive
need=()
for t in git curl python3 node npm; do command -v "$t" >/dev/null || need+=("$t"); done
if [ ${#need[@]} -gt 0 ]; then
  apt-get update -qq
  apt-get install -y -qq git curl python3 nodejs npm ca-certificates >/dev/null
fi
log "node $(node --version), npm $(npm --version)"

log "bipnip-web $WEB_REV"
git clone --quiet https://github.com/SovereignTechnology/bipnip-web "$W/web"
git -C "$W/web" checkout --quiet --detach "$WEB_REV"
[ "$(git -C "$W/web" rev-parse HEAD)" = "$WEB_REV" ]
(cd "$W/web" && npm ci --ignore-scripts --no-audit --no-fund --loglevel=error && npm run --silent build:bipnip >/dev/null)
ls "$W/web/dist/basic.html" "$W/web/dist/pro.html" >/dev/null

log "bipnip-webhost"
cargo build --quiet --release -p bipnip-webhost --manifest-path "$REPO/Cargo.toml" --target-dir "$W/target"

log "start the host (Tor bootstrap)"
"$W/target/release/bipnip-webhost" --dist "$W/web/dist" --listen "127.0.0.1:$PORT" --tor-dir "$W/tor" > "$W/host.log" 2>&1 &
HOST_PID=$!
t=0; until grep -q 'serving' "$W/host.log"; do
  sleep 2; t=$((t+2))
  kill -0 "$HOST_PID" 2>/dev/null || { log "host exited"; cat "$W/host.log"; exit 1; }
  [ "$t" -lt 300 ] || { log "no bootstrap in 300 s"; cat "$W/host.log"; exit 1; }
done
log "$(grep serving "$W/host.log")"

log "coordinators through the host (public info and book only)"
python3 - "$W/web/dist/static/federation.json" "$PORT" <<'PY'
import json, sys, time, urllib.request
fed = json.load(open(sys.argv[1])); port = sys.argv[2]
for alias, e in fed.items():
    for net in ("mainnet", "testnet"):
        if not ((e.get(net) or {}).get("onion")):
            continue
        row = [net, alias]
        for path in ("api/info/", "api/book/"):
            url = f"http://127.0.0.1:{port}/{net}/{alias}/{path}"
            t0 = time.time()
            try:
                with urllib.request.urlopen(urllib.request.Request(url, headers={"Host": f"127.0.0.1:{port}"}), timeout=120) as r:
                    body = r.read()
                    n = len(json.loads(body)) if path == "api/book/" else "ok"
                    row.append(f"{path}={r.status}:{n} ({time.time()-t0:.0f}s)")
            except Exception as ex:
                row.append(f"{path}=ERR {type(ex).__name__}: {str(ex)[:60]} ({time.time()-t0:.0f}s)")
        print("  " + "  ".join(map(str, row)), flush=True)
PY

log "headless Chromium (Playwright $PW)"
mkdir -p "$W/pw"
cp "$HERE/shot.mjs" "$W/pw/"
(cd "$W/pw" && npm init -y >/dev/null && npm i --no-audit --no-fund --loglevel=error "playwright@$PW" >/dev/null \
  && npx --yes "playwright@$PW" install --with-deps chromium >/dev/null 2>&1)
(cd "$W/pw" && node shot.mjs "http://127.0.0.1:$PORT" "$W/garage.png" "$W/offers.png") | tee "$W/summary.json"

log "host log (no headers or bodies are ever logged)"
tail -5 "$W/host.log"
kill "$HOST_PID" 2>/dev/null || true

for shot in garage offers; do
  echo "=== W0 SCREENSHOT $shot BASE64 BEGIN"
  base64 -w 0 "$W/$shot.png"; echo
  echo "=== W0 SCREENSHOT $shot BASE64 END"
done
python3 - "$W/summary.json" <<'PY'
import json, sys
s = json.load(open(sys.argv[1]))
ok = s["rows"] > 3 and s["csp_violations"] == 0 and s["off_origin_requests"] == 0
print("W0 RESULT:", "PASS" if ok else "CHECK", json.dumps({k: s[k] for k in ("rows", "csp_violations", "off_origin_requests")}))
PY
