#!/usr/bin/env bash
# Packs built binaries into a release tarball (R1.28):
# <out>/reef-<version>-<target>.tar.gz, holding reef-<version>-<target>/ with
# `reef`, `reef-daemon`, LICENSE, README.md, and completions/ for bash, zsh,
# and fish. The completions come from running `reef`, so the binaries must
# run on this machine.
#
#   scripts/package.sh <bin-dir> <version> <target> <out-dir>
#
# Prints the tarball's path. The release workflow calls it per target; the
# installer test (`just installer`) calls it on a debug build.

set -euo pipefail

if [[ $# -ne 4 ]]; then
    echo "usage: $0 <bin-dir> <version> <target> <out-dir>" >&2
    exit 2
fi
bin_dir=$1 version=$2 target=$3 out=$4
root=$(cd "$(dirname "$0")/.." && pwd)
name="reef-$version-$target"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name/completions" "$out"
cp "$bin_dir/reef" "$bin_dir/reef-daemon" "$root/LICENSE" "$root/README.md" "$stage/$name/"
reef="$stage/$name/reef"
"$reef" completions bash >"$stage/$name/completions/reef.bash"
"$reef" completions zsh >"$stage/$name/completions/_reef"
"$reef" completions fish >"$stage/$name/completions/reef.fish"

# COPYFILE_DISABLE: macOS tar would add AppleDouble `._` files.
(cd "$stage" && COPYFILE_DISABLE=1 tar -czf "$name.tar.gz" "$name")
mv "$stage/$name.tar.gz" "$out/"
echo "$out/$name.tar.gz"
