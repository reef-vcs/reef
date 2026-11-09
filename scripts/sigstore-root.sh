#!/bin/sh
# Compares the Sigstore trusted root Reef ships (R3.42, ADR 0066) with the one Sigstore publishes
# in its root-signing repository, and with --update replaces it. Exits 1 when they differ and
# --update was not given, so a scheduled job turns red when the vendored root goes stale. The
# daemon never fetches a root itself; a release carries the root it was built with.
set -eu

VENDORED="crates/reef-evidence/sigstore/trusted_root.json"
PUBLISHED="https://raw.githubusercontent.com/sigstore/root-signing/main/targets/trusted_root.json"

cd "$(dirname "$0")/.."
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
curl -fsSL --proto '=https' "$PUBLISHED" -o "$tmp"

digest() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
if [ "$(digest "$tmp")" = "$(digest "$VENDORED")" ]; then
  echo "the vendored Sigstore trusted root is current ($(digest "$VENDORED"))"
  exit 0
fi
if [ "${1:-}" = "--update" ]; then
  cp "$tmp" "$VENDORED"
  echo "updated $VENDORED to $(digest "$VENDORED"); review the diff, run the reef-evidence tests, and commit it"
  exit 0
fi
echo "the vendored Sigstore trusted root is stale: $(digest "$VENDORED") here, $(digest "$tmp") published" >&2
echo "run scripts/sigstore-root.sh --update" >&2
exit 1
