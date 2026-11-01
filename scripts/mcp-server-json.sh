#!/usr/bin/env bash
# Prints the MCP registry's server.json for a release (R2.25):
# packaging/mcp/server.json with one MCPB package per target, each at its
# release URL with the checksum SHA256SUMS gives it. Fails unless there are
# exactly four bundles, each with a checksum.
#
#   scripts/mcp-server-json.sh <version> <SHA256SUMS>

set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <version> <SHA256SUMS>" >&2
    exit 2
fi
version=$1 sums=$2
root=$(cd "$(dirname "$0")/.." && pwd)
template="$root/packaging/mcp/server.json"
if [[ "$(jq -r .version "$template")" != "$version" ]]; then
    echo "error: $template is for $(jq -r .version "$template"), not $version; run just packages" >&2
    exit 1
fi

packages=$(
    grep -E " reef-${version//./\\.}-[a-z0-9_-]+\.mcpb$" "$sums" |
        jq -R --arg version "$version" '
            capture("^(?<sha>[0-9a-f]{64})  (?<file>.+)$")
            | {
                registryType: "mcpb",
                identifier: "https://github.com/reef-vcs/reef/releases/download/v\($version)/\(.file)",
                version: $version,
                fileSha256: .sha,
                transport: {type: "stdio"}
              }' |
        jq -s .
)
count=$(jq length <<<"$packages")
if [[ "$count" -ne 4 ]]; then
    echo "error: $sums lists $count MCPB bundles for $version, not 4" >&2
    exit 1
fi
jq --argjson packages "$packages" '.packages = $packages' "$template"
