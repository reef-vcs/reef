#!/usr/bin/env bash
# Installs a packed reef-vcs tarball alone in a fresh directory and runs the SDK's smoke test
# against REEF_BIN_DIR's daemon, then checks it loads from CommonJS (R2.07). The smoke test is
# copied beside the install: ES modules resolve `reef-vcs` from the importing file's directory.
#
#   REEF_BIN_DIR=target/debug scripts/npm-smoke.sh sdk/typescript/dist-pack/reef-vcs-0.4.2.tgz /tmp/smoke
set -euo pipefail

tarball="$1"
dir="$2"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$dir"
cp "$here/sdk/typescript/tests/smoke.mjs" "$dir/"
cd "$dir"
node --version
npm init -y >/dev/null
npm install --no-save --no-audit --no-fund "$tarball"
node smoke.mjs
node -e 'if (typeof require("reef-vcs").connect !== "function") { console.error("require(\"reef-vcs\") has no connect"); process.exit(1); }'
echo "CommonJS require works"
