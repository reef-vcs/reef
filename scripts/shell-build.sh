#!/usr/bin/env bash
# Builds Reef.app, the desktop app in shell/ (R3.39, docs/desktop.md).
#
#   scripts/shell-build.sh debug               a debug Reef.app; it starts an installed reef
#   scripts/shell-build.sh release <bin-dir> <target>
#                                              Reef.app and its .dmg for <target>, with
#                                              <bin-dir>'s reef and reef-daemon inside
#
# Signing: a debug build signs with APPLE_SIGNING_IDENTITY, else the keychain's first "Apple
# Development" identity, else ad hoc. macOS keeps an app's notification permission across builds
# only for a signature with a team; an ad-hoc app is pinned to its exact code (ADR 0093). A release
# build is ad hoc. It builds into $CARGO_TARGET_DIR/shell (or target/shell) and prints the bundle's
# directory.

set -euo pipefail

mode=${1:-}
root=$(cd "$(dirname "$0")/.." && pwd)
target_dir="${CARGO_TARGET_DIR:-$root/target}/shell"
tauri=(npx --yes @tauri-apps/cli@2.12.6)

# App Transport Security refuses http to a name under localhost unless the app excepts it, and the
# window loads the daemon's page at one (#346, ADR 0094).
check_ats() {
    local plist="$1/Reef.app/Contents/Info.plist" key
    for key in NSIncludesSubdomains NSExceptionAllowsInsecureHTTPLoads; do
        if [[ "$(plutil -extract "NSAppTransportSecurity.NSExceptionDomains.reef\\.localhost.$key" raw -o - "$plist" 2>/dev/null)" != true ]]; then
            echo "$plist does not except reef.localhost ($key) from App Transport Security" >&2
            exit 1
        fi
    done
}

case "$mode" in
    debug)
        if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
            identity=$(security find-identity -v -p codesigning 2>/dev/null |
                sed -n 's/.*"\(Apple Development: [^"]*\)".*/\1/p' | head -1)
            export APPLE_SIGNING_IDENTITY="${identity:--}"
        fi
        echo "signing with: ${APPLE_SIGNING_IDENTITY/#-/ad hoc (notification permission resets at each rebuild)}" >&2
        (cd "$root/shell" && CARGO_TARGET_DIR=$target_dir "${tauri[@]}" build --debug --bundles app >&2)
        check_ats "$target_dir/debug/bundle/macos"
        echo "$target_dir/debug/bundle/macos"
        ;;
    release)
        bin_dir=${2:?usage: $0 release <bin-dir> <target>}
        triple=${3:?usage: $0 release <bin-dir> <target>}
        mkdir -p "$root/shell/binaries" "$target_dir"
        for bin in reef reef-daemon; do
            cp "$bin_dir/$bin" "$root/shell/binaries/$bin-$triple"
        done
        config="$target_dir/release.conf.json"
        printf '{"version": "%s", "bundle": {"externalBin": ["binaries/reef", "binaries/reef-daemon"]}}\n' \
            "$("$root/scripts/version.sh")" >"$config"
        export APPLE_SIGNING_IDENTITY=-
        (cd "$root/shell" && CARGO_TARGET_DIR=$target_dir "${tauri[@]}" build \
            --target "$triple" --config "$config" --bundles app,dmg >&2)
        check_ats "$target_dir/$triple/release/bundle/macos"
        echo "$target_dir/$triple/release/bundle"
        ;;
    *)
        echo "usage: $0 debug | release <bin-dir> <target>" >&2
        exit 2
        ;;
esac
