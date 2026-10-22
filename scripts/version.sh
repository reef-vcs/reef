#!/usr/bin/env bash
# Prints the workspace version, `[workspace.package]`'s `version` in
# Cargo.toml: what release tarballs are named for and a release tag must
# match (R1.28).

set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
version=$(awk '
    /^\[/ { in_package = $0 == "[workspace.package]"; next }
    in_package && /^version *=/ { gsub(/^version *= *"|".*$/, ""); print; exit }
' "$root/Cargo.toml")
[[ -n "$version" ]] || { echo "error: no [workspace.package] version in $root/Cargo.toml" >&2; exit 1; }
echo "$version"
