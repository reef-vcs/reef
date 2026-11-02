#!/usr/bin/env bash
# Checks an MCPB bundle as a client would run it (R2.25): its manifest
# validates and names this release and the target's platform, both binaries
# run, and the server the manifest starts lists the manifest's tools over
# stdio and answers a call through its own `reef-daemon`, autostarted from
# the bundle. Needs node (the pinned mcpb CLI), jq and python3.
#
#   scripts/test-mcpb.sh <bundle.mcpb> <version> <target>

set -euo pipefail

if [[ $# -ne 3 ]]; then
    echo "usage: $0 <bundle.mcpb> <version> <target>" >&2
    exit 2
fi
bundle=$1 version=$2 target=$3
mcpb="@anthropic-ai/mcpb@${MCPB_VERSION:-2.1.2}"
# Short: the daemon's socket lives under the state directory, and a Unix
# socket's path is at most about 100 bytes.
# Resolved, as `reef` resolves its own path to find `reef-daemon` (macOS's
# /tmp is a link).
tmp=$(cd "$(mktemp -d /tmp/reef-mcpb.XXXXXX)" && pwd -P)
state="$tmp/state"
cleanup() {
    REEF_STATE_DIR="$state" "$tmp/x/reef" --no-autostart daemon stop >/dev/null 2>&1 || true
    rm -rf "$tmp"
}
trap cleanup EXIT

fail() { echo "error: $*" >&2; exit 1; }

mkdir -p "$tmp/x"
unzip -q "$bundle" -d "$tmp/x"
manifest="$tmp/x/manifest.json"
npx --yes "$mcpb" validate "$manifest" >/dev/null
case "$target" in
    *-apple-darwin) platform=darwin ;;
    *) platform=linux ;;
esac
[[ "$(jq -r .version "$manifest")" == "$version" ]] || fail "manifest version is not $version"
[[ "$(jq -c .compatibility.platforms "$manifest")" == "[\"$platform\"]" ]] || fail "manifest platform is not $platform"
for bin in reef reef-daemon; do
    [[ -x "$tmp/x/$bin" ]] || fail "$bin is missing or not executable"
    "$tmp/x/$bin" --version | grep -qE "^$bin ${version//./\\.}( |$)" || fail "$bin does not print $version"
done

# The server as the manifest starts it, with `${__dirname}` the bundle.
command=$(jq -r '.server.mcp_config.command' "$manifest")
# shellcheck disable=SC2016 # the manifest's literal placeholder
command=${command//'${__dirname}'/$tmp/x}
# Not mapfile: macOS's bash is 3.2.
args=()
while IFS= read -r arg; do args+=("$arg"); done < <(jq -r '.server.mcp_config.args[]' "$manifest")
jq -r '.tools[].name' "$manifest" >"$tmp/expected"
env -u REEF_SOCKET -u REEF_CONFIG -u REEF_WORKSPACE REEF_STATE_DIR="$state" HOME="$tmp" \
    python3 -I - "$command" "${args[@]}" >"$tmp/result" <<'PY'
import json, subprocess, sys

server = subprocess.Popen(sys.argv[1:], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

def send(message):
    server.stdin.write(json.dumps(message) + "\n")
    server.stdin.flush()

def answer(id):
    for line in server.stdout:
        message = json.loads(line)
        if message.get("id") == id:
            return message
    raise SystemExit(f"the server closed before answering {id}")

send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-06-18", "capabilities": {},
    "clientInfo": {"name": "test-mcpb", "version": "0"}}})
answer(1)
send({"jsonrpc": "2.0", "method": "notifications/initialized"})
send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
tools = [tool["name"] for tool in answer(2)["result"]["tools"]]
send({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
      "params": {"name": "list_repositories", "arguments": {}}})
call = answer(3)["result"]
server.stdin.close()
server.wait(timeout=30)
print(json.dumps({"tools": tools, "is_error": call.get("isError", False)}))
PY
diff <(jq -r '.tools[]' "$tmp/result") "$tmp/expected" || fail "the server's tools differ from the manifest's"
[[ "$(jq -r .is_error "$tmp/result")" == false ]] || fail "list_repositories failed: $(cat "$tmp/result")"

# The call autostarted the bundle's own daemon.
pid=$(REEF_STATE_DIR="$state" "$tmp/x/reef" --no-autostart daemon status -o json | jq -r .pid)
if [[ -e "/proc/$pid/exe" ]]; then
    exe=$(readlink "/proc/$pid/exe")
else
    exe=$(ps -o comm= -p "$pid")
fi
[[ "$exe" == "$tmp/x/reef-daemon" ]] || fail "the daemon is $exe, not the bundle's"
echo "ok: $(basename "$bundle"): $(wc -l <"$tmp/expected" | tr -d ' ') tools, daemon $exe"
