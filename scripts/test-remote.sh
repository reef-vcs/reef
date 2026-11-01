#!/usr/bin/env bash
# Phase 2's exit criterion for remote access (R2.08): a daemon in one
# container is reached from another, on its own network address, over TLS
# with a token, serving the API and the web UI.
#
#   scripts/test-remote.sh REEF REEF_DAEMON [--real-app]
#
# REEF and REEF_DAEMON are Linux binaries for this machine's architecture
# (CI: target/debug). Both containers are ubuntu:24.04 with the binaries
# mounted read-only, running as this user, on a Docker network of their
# own. With --real-app, `/` must name the built app's hashed script, not the
# placeholder (the release's install job). Needs docker, git, jq, curl and
# openssl on the host.

set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 REEF REEF_DAEMON [--real-app]" >&2
    exit 2
fi
reef=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
daemon_bin=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
real_app=${3:-}

tmp=$(mktemp -d)
id=reef-remote-$$
net=$id-net
daemon=$id-daemon
client=$id-client
cleanup() {
    docker rm -f "$daemon" "$client" >/dev/null 2>&1 || true
    docker network rm "$net" >/dev/null 2>&1 || true
    rm -rf "$tmp"
}
trap cleanup EXIT

fail() {
    echo "test-remote: $*" >&2
    docker logs "$daemon" 2>&1 | tail -n 40 >&2 || true
    exit 1
}

user="$(id -u):$(id -g)"
mkdir -p "$tmp/bin" "$tmp/state" "$tmp/client"
cp "$reef" "$tmp/bin/reef"
cp "$daemon_bin" "$tmp/bin/reef-daemon"
chmod 0700 "$tmp/state" "$tmp/client"
cat >"$tmp/state/config.toml" <<'EOF'
[remote]
listen = "0.0.0.0:7470"
names = ["daemon"]
EOF

# A fixture repository the daemon's user owns.
git init -q "$tmp/repo"
git -C "$tmp/repo" -c user.name=reef -c user.email=reef@example.com \
    commit -q --allow-empty -m "fixture"

docker network create "$net" >/dev/null
# The daemon: its own container and address, the port published on the
# host's loopback only for curl below. The socket stays in the container: a
# bind mount from a macOS host cannot hold one.
docker run -d --name "$daemon" --network "$net" --network-alias daemon \
    --user "$user" -e HOME=/tmp -e REEF_STATE_DIR=/state -e REEF_SOCKET=/tmp/reef/daemon.sock \
    -v "$tmp/bin:/opt/reef:ro" -v "$tmp/state:/state" -v "$tmp/repo:/repo" \
    -p 127.0.0.1::7470 \
    ubuntu:24.04 /opt/reef/reef-daemon --foreground --remote >/dev/null

on_daemon() {
    docker exec -e REEF_STATE_DIR=/state -e REEF_SOCKET=/tmp/reef/daemon.sock -e HOME=/tmp \
        "$daemon" /opt/reef/reef "$@"
}

for _ in $(seq 1 100); do
    state=$(on_daemon -o json daemon status 2>/dev/null | jq -r '.remote.state // empty' || true)
    [[ $state == listening ]] && break
    sleep 0.1
done
[[ ${state:-} == listening ]] || fail "the remote listener never listened (state: ${state:-none})"

status=$(on_daemon -o json daemon status)
fingerprint=$(jq -r .remote.certificate.fingerprint <<<"$status")
repo=$(on_daemon -o json repo register /repo | jq -r '.repository.id // .id')
write=$(on_daemon -o json remote token create writer --scope write | jq -r .token)
read=$(on_daemon -o json remote token create reader | jq -r .token)
revoked=$(on_daemon -o json remote token create gone | jq -r .token)
on_daemon -o json remote token revoke gone >/dev/null
[[ $fingerprint == sha256:* && $write == reef_* && -n $repo ]] || fail "setup: $fingerprint $repo"

# The client: another container on the network, reaching https://daemon:7470.
docker run -d --name "$client" --network "$net" --user "$user" -e HOME=/tmp \
    -e REEF_STATE_DIR=/client -e NO_COLOR=1 \
    -v "$tmp/bin:/opt/reef:ro" -v "$tmp/client:/client" \
    ubuntu:24.04 sleep 600 >/dev/null

on_client() {
    docker exec -i "$client" /opt/reef/reef --no-autostart "$@"
}

expect_exit() {
    local want=$1
    shift
    local got=0
    "$@" >/dev/null 2>&1 || got=$?
    [[ $got == "$want" ]] || fail "expected exit $want, got $got: $*"
}

url=https://daemon:7470
# A wrong pin is refused before any request; so is no pin at all.
expect_exit 7 on_client remote add bad "$url" --fingerprint "sha256:$(printf '0%.0s' {1..64})" \
    --token-file - <<<"$write"
expect_exit 7 on_client remote add bad "$url" --token-file - <<<"$write"
# The write token, pinned.
on_client remote add vm "$url" --fingerprint "$fingerprint" --token-file - <<<"$write" >/dev/null
ws=$(on_client --remote vm -o json ws create --repo "$repo" --purpose "from another host" | jq -r .id)
[[ -n $ws && $ws != null ]] || fail "ws create returned no workspace"
on_client --remote vm -o json status --workspace "$ws" >/dev/null
on_client --remote vm -o json describe --workspace "$ws" -m "described from another host" >/dev/null
named=$(on_client --remote vm -o json op log --repo "$repo" --limit 5 |
    jq -r '[.[] | select(.kind == "describe")][0].remote.token_name')
[[ $named == writer ]] || fail "the describe's operation names token '$named', not writer"
# A read token cannot write; a revoked one cannot do anything.
docker exec -i -e REEF_REMOTE="$url" -e REEF_REMOTE_FINGERPRINT="$fingerprint" \
    -e REEF_REMOTE_TOKEN="$read" "$client" /opt/reef/reef -o json ws list --repo "$repo" >/dev/null ||
    fail "the read token could not list workspaces"
expect_exit 7 docker exec -e REEF_REMOTE="$url" -e REEF_REMOTE_FINGERPRINT="$fingerprint" \
    -e REEF_REMOTE_TOKEN="$read" "$client" /opt/reef/reef -o json describe --workspace "$ws" -m no
expect_exit 7 docker exec -e REEF_REMOTE="$url" -e REEF_REMOTE_FINGERPRINT="$fingerprint" \
    -e REEF_REMOTE_TOKEN="$revoked" "$client" /opt/reef/reef -o json ws list --repo "$repo"
# Local-only commands refuse before connecting.
expect_exit 9 on_client --remote vm here
on_client --remote vm -o json ws release "$ws" >/dev/null

# The web UI over TLS, the key pinned as curl pins it.
port=$(docker port "$daemon" 7470/tcp | head -n 1 | sed 's/.*://')
hex=${fingerprint#sha256:}
# shellcheck disable=SC2001,SC2059 # the format is the bytes, as \x escapes
pin=$(printf "$(sed 's/../\\x&/g' <<<"$hex")" | openssl base64 -A)
page=$(curl -sS --fail --resolve "daemon:$port:127.0.0.1" -k \
    --pinnedpubkey "sha256//$pin" -D "$tmp/headers" "https://daemon:$port/")
grep -qi '^content-security-policy:' "$tmp/headers" || fail "/ has no security headers"
if [[ $real_app == --real-app ]]; then
    grep -Eq 'assets/index-[A-Za-z0-9_-]+\.js' <<<"$page" || fail "/ is not the built app"
fi
# A wrong pin: curl refuses.
if curl -sS --resolve "daemon:$port:127.0.0.1" -k \
    --pinnedpubkey "sha256//$(head -c 32 /dev/zero | openssl base64 -A)" \
    "https://daemon:$port/" >/dev/null 2>&1; then
    fail "curl accepted a wrong pin"
fi

echo "test-remote: ok (daemon and client in separate containers, $fingerprint)"
