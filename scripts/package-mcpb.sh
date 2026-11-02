#!/usr/bin/env bash
# Packs built binaries into an MCPB bundle for the MCP registry (R2.25):
# <out>/reef-<version>-<target>.mcpb, a zip of manifest.json (from
# packaging/mcpb/manifest.json, with this target's platform), `reef`,
# `reef-daemon`, LICENSE and README.md. The bundle runs its own `reef mcp`,
# which finds `reef-daemon` beside it. Needs node for the pinned mcpb CLI.
#
#   scripts/package-mcpb.sh <bin-dir> <version> <target> <out-dir>
#
# Prints the bundle's path. The release workflow calls it per target.

set -euo pipefail

if [[ $# -ne 4 ]]; then
    echo "usage: $0 <bin-dir> <version> <target> <out-dir>" >&2
    exit 2
fi
bin_dir=$1 version=$2 target=$3 out=$4
root=$(cd "$(dirname "$0")/.." && pwd)
mcpb="@anthropic-ai/mcpb@${MCPB_VERSION:-2.1.2}"
name="reef-$version-$target"

case "$target" in
    *-apple-darwin) platform=darwin ;;
    *-linux-*) platform=linux ;;
    *) echo "error: no MCPB platform for $target" >&2; exit 2 ;;
esac

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name" "$out"
manifest="$root/packaging/mcpb/manifest.json"
if [[ "$(jq -r .version "$manifest")" != "$version" ]]; then
    echo "error: $manifest is for $(jq -r .version "$manifest"), not $version; run just packages" >&2
    exit 1
fi
jq --arg platform "$platform" '.compatibility.platforms = [$platform]' "$manifest" >"$stage/$name/manifest.json"
cp "$bin_dir/reef" "$bin_dir/reef-daemon" "$root/LICENSE" "$root/README.md" "$stage/$name/"
npx --yes "$mcpb" validate "$stage/$name/manifest.json" >&2
npx --yes "$mcpb" pack "$stage/$name" "$stage/$name.mcpb" >&2
mv "$stage/$name.mcpb" "$out/"
echo "$out/$name.mcpb"
