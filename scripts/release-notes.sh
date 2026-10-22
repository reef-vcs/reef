#!/usr/bin/env bash
# Prints CHANGELOG.md's section for a version, the GitHub Release's notes
# (R1.28). The section runs from its `## [<version>]` or `## <version>`
# heading to the next `## ` heading. A missing or empty section fails, and so
# does the release. Read-only.
#
#   scripts/release-notes.sh <version> [changelog]

set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
    echo "usage: $0 <version> [changelog]" >&2
    exit 2
fi
version=${1#v}
changelog=${2:-$(cd "$(dirname "$0")/.." && pwd)/CHANGELOG.md}
[[ -f "$changelog" ]] || { echo "error: no $changelog" >&2; exit 1; }

notes=$(awk -v version="$version" '
    /^## / {
        if (found) exit
        heading = $2
        gsub(/^\[|\]$/, "", heading)
        found = heading == version
        next
    }
    found
' "$changelog" | sed -e '/./,$!d')
# `sed` above drops leading blank lines; `$(...)` drops trailing ones.
if [[ -z "$notes" ]]; then
    echo "error: $changelog has no section for $version (a \`## [$version]\` heading)" >&2
    exit 1
fi
printf '%s\n' "$notes"
