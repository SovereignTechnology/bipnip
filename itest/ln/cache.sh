#!/usr/bin/env bash
# What the itest scripts keep between runs: downloads, and compiled code (the target directories).
# Sourced by itest/ln/run-*.sh and itest/soak/run-soak.sh after they set W (their work directory, which
# they wipe at the start) and before they download or build anything.
#
# kata-run on alpha's agent runner keeps a persistent cache per repo, and says so by pointing CARGO_HOME at
# /cache/kr/<repo>/cargo (<repo>/target links to the same place). The cache kept here is the directory
# beside it, /cache/kr/<repo>/itest: it goes with the rest when kata-run drops a repo's cache (a job killed
# mid-build, or the volume running low). Anywhere else, in a fresh VM at home or on a machine with nothing
# persistent, the cache is inside W and is wiped with it, so a run is exactly what it was before.
# BIPNIP_ITEST_CACHE puts it somewhere else.
#
#   C                    the cache directory
#   $C/target, target-ws, target-d, target-ldk, target-m     cargo's target directories
#   fetch URL SHA256 DEST    download URL to DEST and check it; a copy is kept in $C/dl, named by its hash,
#                            and used next time when it still matches

if [ -n "${BIPNIP_ITEST_CACHE:-}" ]; then
  C=$BIPNIP_ITEST_CACHE
elif [[ "${CARGO_HOME:-}" =~ ^/cache/kr/[A-Za-z0-9._-]+/cargo$ ]]; then
  C=${CARGO_HOME%/cargo}/itest
else
  C=$W/cache
fi
mkdir -p "$C/dl"

fetch() {
  local url=$1 sha=$2 dest=$3 kept="$C/dl/$2"
  if [ -f "$kept" ] && echo "$sha  $kept" | sha256sum -c --quiet 2>/dev/null; then
    cp "$kept" "$dest"
    return 0
  fi
  # Explicit returns: the check must fail the call where `set -e` is off too (an `if fetch …`), as the
  # one-line fetch these scripts had before it did.
  curl -fsSL --retry 3 -o "$dest" "$url" || return 1
  echo "$sha  $dest" | sha256sum -c --quiet || return 1
  # Kept only once checked, and moved into place whole. A name an earlier job left at $kept.new (a link to
  # some file this job may write) is removed first, so cp makes a new file instead of writing through it.
  rm -f "$kept.new" && cp "$dest" "$kept.new" && mv -f "$kept.new" "$kept"
}
