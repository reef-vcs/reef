#!/usr/bin/env bash
# Renders packaging/homebrew/reef.rb.tmpl for a release and pushes it to the
# tap, reef-vcs/homebrew-tap, as one commit on `main` (R1.28). The release
# workflow runs it after publishing, once `brew install` of the same formula
# has passed; run it by hand when that step failed.
#
#   scripts/update-tap.sh <version> <SHA256SUMS> [--base-url URL] [--out FILE | --dry-run]
#
#   --base-url URL  where the tarballs are; default the version's GitHub Release.
#   --out FILE      write the formula to FILE and stop (the workflow's brew test).
#   --dry-run       print the formula and the commit it would push; change nothing.
#
# Publishing pushes over SSH to TAP_REMOTE (default
# git@github.com:reef-vcs/homebrew-tap.git), so it needs a key with write
# access to the tap: your own by hand, the tap's deploy key in the workflow
# (HOMEBREW_TAP_DEPLOY_KEY). The tap must already have a `main` branch.
# Rerunning for a version the tap already has changes nothing.

set -euo pipefail
# bash 5.2 would expand `&` in a substitution's replacement to the match.
shopt -u patsub_replacement 2>/dev/null || true

TAP_REMOTE=${TAP_REMOTE:-git@github.com:reef-vcs/homebrew-tap.git}
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl)

usage() {
    echo "usage: $0 <version> <SHA256SUMS> [--base-url URL] [--out FILE | --dry-run]" >&2
    exit 2
}

[[ $# -ge 2 ]] || usage
version=${1#v} sums=$2
shift 2
base_url="https://github.com/reef-vcs/reef/releases/download/v$version" out="" dry_run=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --base-url) [[ $# -ge 2 ]] || usage; base_url=${2%/}; shift 2 ;;
        --out) [[ $# -ge 2 ]] || usage; out=$2; shift 2 ;;
        --dry-run) dry_run=true; shift ;;
        *) usage ;;
    esac
done
root=$(cd "$(dirname "$0")/.." && pwd)

formula=$(<"$root/packaging/homebrew/reef.rb.tmpl")
formula=${formula//@VERSION@/$version}
formula=${formula//@BASE_URL@/$base_url}
for target in "${TARGETS[@]}"; do
    name="reef-$version-$target.tar.gz"
    sha=$(awk -v name="$name" '{ file = $2; sub(/^\*/, "", file) } file == name { print $1 }' "$sums")
    if [[ ! "$sha" =~ ^[0-9a-f]{64}$ ]]; then
        echo "error: $sums has no checksum for $name" >&2
        exit 1
    fi
    formula=${formula//@SHA256_${target}@/$sha}
done
if grep -q '@[A-Za-z0-9_-]*@' <<<"$formula"; then
    echo "error: the template has a placeholder this script does not fill" >&2
    exit 1
fi

if [[ -n "$out" ]]; then
    printf '%s\n' "$formula" >"$out"
    exit 0
fi

message="reef $version

Rendered by scripts/update-tap.sh from
https://github.com/reef-vcs/reef/blob/v$version/packaging/homebrew/reef.rb.tmpl"
if $dry_run; then
    printf '%s\n' "$formula"
    echo "dry run: would push \"reef $version\" to main on $TAP_REMOTE, writing Formula/reef.rb as above" >&2
    exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git clone --quiet --depth 1 --branch main "$TAP_REMOTE" "$work/tap"
cd "$work/tap"
mkdir -p Formula
printf '%s\n' "$formula" >Formula/reef.rb
git add Formula/reef.rb
if git diff --cached --quiet; then
    echo "the tap's Formula/reef.rb is already reef $version" >&2
    exit 0
fi
git -c user.name="reef release" -c user.email="reef-release@users.noreply.github.com" \
    commit --quiet -m "$message"
# Not forced: if main moved since the clone, this fails and a rerun picks it up.
git push --quiet origin HEAD:main
echo "pushed reef $version to $TAP_REMOTE: $(git rev-parse --short HEAD)" >&2
