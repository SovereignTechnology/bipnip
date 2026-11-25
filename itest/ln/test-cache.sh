#!/usr/bin/env bash
# fetch() and the cache directory of itest/ln/cache.sh, against file:// URLs, in a scratch directory:
#   a miss downloads and keeps; a hit does not download; a kept file that no longer matches is fetched
#   again and replaced; a wrong hash is refused and nothing is kept; a link planted at $kept.new is not
#   written through; where C goes (inside W, under /cache/kr/<repo>/itest, or BIPNIP_ITEST_CACHE).
# Run it in Kata: kata-run -- bash itest/ln/test-cache.sh
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
T=$(mktemp -d); W=$T/w; mkdir -p "$W"; cd "$T"
unset BIPNIP_ITEST_CACHE CARGO_HOME
cache_dir() { ( W=$W; . "$HERE/cache.sh"; printf '%s\n' "$C" ); }
[ "$(cache_dir)" = "$W/cache" ] || { echo "FAIL: C is $(cache_dir) with nothing persistent"; exit 1; }
[ "$(CARGO_HOME=/cache/kr/bipnip/cargo cache_dir)" = /cache/kr/bipnip/itest ] || { echo "FAIL: C under kata-run's cache"; exit 1; }
[ "$(CARGO_HOME=/elsewhere/cargo cache_dir)" = "$W/cache" ] || { echo "FAIL: C with a CARGO_HOME that is not kata-run's"; exit 1; }
[ "$(BIPNIP_ITEST_CACHE=$T/override cache_dir)" = "$T/override" ] || { echo "FAIL: C from BIPNIP_ITEST_CACHE"; exit 1; }
. "$HERE/cache.sh"
printf 'payload one\n' > src1; SHA=$(sha256sum src1 | cut -c1-64)
fetch "file://$T/src1" "$SHA" "$T/out1"; cmp src1 out1 || { echo FAIL miss; exit 1; }
[ -f "$C/dl/$SHA" ] || { echo "FAIL: not kept"; exit 1; }
rm src1; fetch "file://$T/src1-gone" "$SHA" "$T/out2"; cmp out1 out2 || { echo "FAIL: hit did not use the kept copy"; exit 1; }
printf 'tampered\n' > "$C/dl/$SHA"; cp out1 src1
fetch "file://$T/src1" "$SHA" "$T/out3"; cmp out1 out3 && [ "$(sha256sum < "$C/dl/$SHA" | cut -c1-64)" = "$SHA" ] || { echo "FAIL: a tampered copy was used or kept"; exit 1; }
if (fetch "file://$T/src1" "0000000000000000000000000000000000000000000000000000000000000000" "$T/out4") 2>/dev/null; then echo "FAIL: a wrong hash was accepted"; exit 1; fi
[ ! -e "$C/dl/0000000000000000000000000000000000000000000000000000000000000000" ] || { echo "FAIL: a wrong hash was kept"; exit 1; }
printf 'victim\n' > "$T/victim"; rm -f "$C/dl/$SHA"; ln -s "$T/victim" "$C/dl/$SHA.new"
fetch "file://$T/src1" "$SHA" "$T/out5"
[ "$(cat "$T/victim")" = victim ] || { echo "FAIL: a link at .new was written through"; exit 1; }
[ ! -L "$C/dl/$SHA" ] && cmp out1 "$C/dl/$SHA" || { echo "FAIL: kept copy wrong after a planted link"; exit 1; }
echo "FETCH TEST PASS"
