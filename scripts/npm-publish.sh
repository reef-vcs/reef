#!/usr/bin/env bash
# Publishes the TypeScript SDK's packed tarball to npm (release.yml's `npm` job), or with
# --dry-run checks the same command line without publishing (its `typescript-sdk` job, every run).
# The tag is `next` for a prerelease version (one with `-`, which npm 11 refuses to publish
# untagged) and `latest` otherwise. A real publish authenticates by trusted publishing and gets
# provenance; a dry run needs no credentials, so it checks neither.
#
#   scripts/npm-publish.sh sdk/typescript/dist-pack/reef-vcs-0.4.1.tgz --dry-run
#
# The tarball is passed to npm as an absolute path. npm reads a relative path with one slash, such
# as `dist/reef-vcs-0.4.0.tgz`, as a GitHub repository (`user/repo`) and fails in `git ls-remote`:
# 0.4.0 never reached npm that way, while the dry run, on a path with three slashes, passed.
set -euo pipefail

usage() { echo "usage: $0 TARBALL [--dry-run]" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }
[[ $# -ge 1 && $# -le 2 ]] || usage
tarball="$1"
dry_run=false
if [[ $# -eq 2 ]]; then
    [[ "$2" == --dry-run ]] || usage
    dry_run=true
fi
[[ -f "$tarball" ]] || die "no tarball at $tarball"
tarball="$(cd "$(dirname "$tarball")" && pwd)/$(basename "$tarball")"

# The name and version npm publishes are the manifest's; the file name must agree, so the tag
# below is chosen for the version that goes out.
manifest="$(tar -xzOf "$tarball" package/package.json)" || die "$tarball has no package/package.json"
name="$(node -p 'JSON.parse(process.argv[1]).name' "$manifest")"
version="$(node -p 'JSON.parse(process.argv[1]).version' "$manifest")"
[[ "$(basename "$tarball")" == "$name-$version.tgz" ]] ||
    die "$tarball holds $name@$version; npm pack names that $name-$version.tgz"
tag=latest
if [[ "$version" == *-* ]]; then
    tag=next
fi

args=(publish "$tarball" --access public --tag "$tag")
if $dry_run; then
    args+=(--dry-run --provenance=false)
    # npm refuses even a dry run of a version it already has. That is every dry run after a
    # release until the next version bump, so let it through: the dry run checks the command
    # line and the tarball, and a real publish of that version still fails.
    if [[ "$(npm view "$name@$version" version 2>/dev/null || true)" == "$version" ]]; then
        echo "$name@$version is already on npm; dry run with --force"
        args+=(--force)
    fi
fi
echo "npm ${args[*]}"
npm "${args[@]}"
