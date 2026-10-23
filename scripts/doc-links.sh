#!/usr/bin/env bash
# Checks that every relative link in the repository's Markdown resolves to a
# file or directory (R1.24). Links in fenced code blocks and inline code are
# examples, not links, and are skipped; so are external links (with a
# scheme) and anchors (`#...`, and the anchor part of `file.md#...`). Prints
# each broken link as `file:line: target` and exits 1 if there are any.
#
#   scripts/doc-links.sh   # from the repository root; `just doc-links`

set -euo pipefail

files() {
    if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        # Tracked and untracked, so a new page is checked before `git add`.
        git ls-files --cached --others --exclude-standard -- '*.md'
    else
        # A checkout without git metadata (a Reef workspace, a tarball).
        find . -name '*.md' -not -path './target/*' -not -path '*/node_modules/*' | sed 's|^\./||'
    fi
}

broken=0
while IFS= read -r file; do
    dir=$(dirname "$file")
    while IFS=$'\t' read -r line target; do
        if [ ! -e "$dir/$target" ]; then
            echo "$file:$line: $target"
            broken=1
        fi
    done < <(awk '
        /^[[:space:]]*(```|~~~)/ { fenced = !fenced; next }
        fenced { next }
        {
            text = $0
            gsub(/`[^`]*`/, "", text)
            while (match(text, /\]\([^)[:space:]]+([[:space:]]+"[^"]*")?\)/)) {
                link = substr(text, RSTART + 2, RLENGTH - 3)
                text = substr(text, RSTART + RLENGTH)
                sub(/[[:space:]].*/, "", link)
                if (link ~ /^[a-zA-Z][a-zA-Z0-9+.-]*:/ || link ~ /^#/) continue
                sub(/#.*/, "", link)
                if (link != "") print NR "\t" link
            }
        }' "$file")
done < <(files)

exit "$broken"
