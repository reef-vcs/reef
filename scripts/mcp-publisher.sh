#!/usr/bin/env bash
# Downloads the MCP registry's publisher, MCP_PUBLISHER_VERSION (a tag such
# as v1.8.7), for this machine into <dir>, after cosign checks it was built
# by the registry's own release workflow at that tag (R2.25). Prints its
# path. Needs cosign.
#
#   scripts/mcp-publisher.sh <dir>

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <dir>" >&2
    exit 2
fi
dir=$1
version=${MCP_PUBLISHER_VERSION:?set MCP_PUBLISHER_VERSION, such as v1.8.7}
os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')
archive="mcp-publisher_${os}_${arch}.tar.gz"
base="https://github.com/modelcontextprotocol/registry/releases/download/$version"

mkdir -p "$dir"
cd "$dir"
curl -fsSL --output "$archive" "$base/$archive"
curl -fsSL --output "$archive.sigstore.json" "$base/$archive.sigstore.json"
cosign verify-blob \
    --bundle "$archive.sigstore.json" \
    --certificate-identity "https://github.com/modelcontextprotocol/registry/.github/workflows/release.yml@refs/tags/$version" \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    "$archive" >&2
tar -xzf "$archive" mcp-publisher
rm -f "$archive" "$archive.sigstore.json"
echo "$PWD/mcp-publisher"
