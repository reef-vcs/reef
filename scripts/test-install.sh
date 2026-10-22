#!/usr/bin/env bash
# Runs install.sh unattended against local tarballs (R1.28) and checks what
# it installed, then that it refuses a tarball whose checksum is wrong and a
# version the release does not have. Installs into a scratch HOME.
#
#   scripts/test-install.sh --build-dir DIR   package DIR's reef and reef-daemon for
#                                             this machine first (CI, `just installer`)
#   scripts/test-install.sh --dist DIR        the release workflow's tarballs and SHA256SUMS

set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
version=$("$root/scripts/version.sh")
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

case "${1:-}:$#" in
    --build-dir:2)
        # A debug build is for the host's target, which on Linux is gnu, not
        # the musl the installer would pick, so name it and pass it on.
        target=$(rustc -vV | sed -n 's/^host: //p')
        export REEF_TARGET=$target
        dist="$tmp/dist"
        "$root/scripts/package.sh" "$2" "$version" "$target" "$dist" >/dev/null
        (cd "$dist" && sha256 ./*.tar.gz | sed 's| \./| |' >SHA256SUMS)
        commit='([0-9a-f]{12}|unknown)'
        ;;
    --dist:2)
        dist=$(cd "$2" && pwd)
        target=""
        commit='[0-9a-f]{12}'
        ;;
    *)
        echo "usage: $0 --build-dir DIR | --dist DIR" >&2
        exit 2
        ;;
esac

run_installer() {
    env HOME="$tmp/home" PATH=/usr/bin:/bin REEF_INSTALL_URL="file://$dist" "$@" \
        sh "$root/install.sh" </dev/null 2>"$tmp/log"
}

run_installer || { cat "$tmp/log"; exit 1; }
cat "$tmp/log"
installed=$("$tmp/home/.local/bin/reef" --version)
"$tmp/home/.local/bin/reef-daemon" --version >/dev/null
[[ "$installed" =~ ^reef\ ${version//./\\.}\ \($commit,\ ${target:-[a-z0-9_]+-[a-z0-9_-]+}\)$ ]] ||
    { echo "error: unexpected version: $installed" >&2; exit 1; }
grep -q 'is not on PATH' "$tmp/log" || { echo "error: no PATH advice" >&2; exit 1; }
echo "ok: $installed"

if run_installer REEF_VERSION=0.0.0-missing; then
    echo "error: installed a version that is not there" >&2
    exit 1
fi
grep -q 'no Reef 0.0.0-missing for' "$tmp/log" || { cat "$tmp/log"; exit 1; }
echo "ok: a missing version fails"

sed -E 's/^[0-9a-f]{64}/0000000000000000000000000000000000000000000000000000000000000000/' \
    "$dist/SHA256SUMS" >"$tmp/SHA256SUMS"
cp "$dist"/*.tar.gz "$tmp/"
if dist=$tmp run_installer; then
    echo "error: installed a tarball with the wrong checksum" >&2
    exit 1
fi
grep -q 'checksum mismatch' "$tmp/log" || { cat "$tmp/log"; exit 1; }
echo "ok: a wrong checksum fails"
