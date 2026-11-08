#!/usr/bin/env bash
# Installs Reef into each harness the way its quickstart line does, from
# this checkout's packages, and checks what the harness then has (R2.25):
#
#   Claude Code  the marketplace and plugin validate (--strict), the plugin
#                installs with its MCP server and skill, its edit hook, run as
#                Claude Code runs it, declares an edit in a workspace, and its
#                prompt hook declares the prompt's hash, the session and the
#                harness there, prints nothing, and exits 0 with no daemon
#   Codex        the Agent Plugins tree installs, its MCP server starts with
#                every tool and its skill reaches the prompt; and
#                `reef init --agents codex --user` configures the server
#   opencode     `reef init --agents opencode --user` configures the server
#                and the skill, which opencode lists once
#
# No harness is signed in, and none needs to be. The marketplaces name the
# release tag, which a pull request's run does not have yet, so each is
# copied with its plugin's source rewritten to the checkout's tree. Every
# harness runs in a scratch HOME. Needs node (npx), jq and git.
#
#   scripts/test-harness-packages.sh <bin-dir>   the reef and reef-daemon to test
#
# The harness versions come from CLAUDE_CODE_VERSION, CODEX_VERSION and
# OPENCODE_VERSION, pinned in release.yml.

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <bin-dir>" >&2
    exit 2
fi
bin=$(cd "$1" && pwd)
root=$(cd "$(dirname "$0")/.." && pwd)
# Short, for the daemon's socket under the state directory.
tmp=$(cd "$(mktemp -d /tmp/reef-harness.XXXXXX)" && pwd -P)
state="$tmp/state"
# Never the script's result: on Linux a workspace is a mount until it is
# released, so it is released before the daemon stops, and what is left of
# the scratch directory is only reported.
cleanup() {
    local status=$?
    if [[ -n "${ws:-}" ]]; then
        REEF_STATE_DIR="$state" "$bin/reef" --no-autostart ws release "$ws" >/dev/null 2>&1 || true
    fi
    REEF_STATE_DIR="$state" "$bin/reef" --no-autostart daemon stop >/dev/null 2>&1 || true
    if ! rm -rf "$tmp" 2>/dev/null; then
        echo "note: could not remove all of $tmp" >&2
    fi
    exit "$status"
}
trap cleanup EXIT

fail() { echo "error: $*" >&2; exit 1; }

# `reef init` in a scratch directory of its own, never this checkout.
init() {
    local dir
    dir=$(mktemp -d "$tmp/init.XXXXXX")
    "$bin/reef" --no-register init "$dir" "$@" -o json >/dev/null
}
step() { echo "--- $*"; }

# The harnesses, installed once into the scratch directory.
step "installing harnesses"
npm install --prefix "$tmp/npm" --no-fund --no-audit --silent \
    "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION:?}" \
    "@openai/codex@${CODEX_VERSION:?}" \
    "opencode-ai@${OPENCODE_VERSION:?}"
claude="$tmp/npm/node_modules/.bin/claude"
codex="$tmp/npm/node_modules/.bin/codex"
opencode="$tmp/npm/node_modules/.bin/opencode"

# A harness's environment: a scratch home, this `reef` first on PATH, and
# nothing of the machine's own agent configuration.
harness_env() {
    local home=$1
    shift
    mkdir -p "$home"
    env -u CLAUDECODE -u CLAUDE_CODE_ENTRYPOINT -u ANTHROPIC_API_KEY -u OPENAI_API_KEY \
        -u CODEX_HOME -u XDG_CONFIG_HOME -u REEF_SOCKET -u REEF_WORKSPACE \
        -u AI_AGENT -u CLAUDE_CODE_SESSION_ID -u REEF_MODEL -u REEF_TASK \
        HOME="$home" CLAUDE_CONFIG_DIR="$home/.claude" REEF_STATE_DIR="$state" \
        XDG_DATA_HOME="$home/.local/share" XDG_CACHE_HOME="$home/.cache" \
        XDG_STATE_HOME="$home/.local/state" PATH="$bin:$PATH" "$@"
}

# A copy of the marketplace at `$1` (relative to the root) with every plugin's
# source rewritten by the jq filter `$2`, which sees the entry's tree as
# `$path` (`./plugins/...`), and the trees it names.
marketplace_copy() {
    local file=$1 filter=$2 copy="$tmp/marketplace-${1//\//-}"
    mkdir -p "$copy/$(dirname "$file")"
    cp -R "$root/plugins" "$copy/"
    jq ".plugins |= map((\"./\" + .source.path) as \$path | .source = ($filter))" "$root/$file" >"$copy/$file"
    echo "$copy"
}

step "Claude Code $("$claude" --version)"
(cd "$root" && harness_env "$tmp/claude-validate" "$claude" plugin validate . --strict)
harness_env "$tmp/claude-validate" "$claude" plugin validate "$root/plugins/claude-code" --strict
harness_env "$tmp/claude-validate" "$claude" plugin validate "$root/plugins/claude-code-workspaces" --strict
# shellcheck disable=SC2016 # a jq variable, not the shell's
market=$(marketplace_copy .claude-plugin/marketplace.json '$path')
home="$tmp/claude-home"
harness_env "$home" "$claude" plugin install reef --marketplace "$market"
# The opt-in workspace provider plugin (R2.26), from the same marketplace.
harness_env "$home" "$claude" plugin install reef-workspaces --marketplace "$market"
harness_env "$home" "$claude" plugin list --json >"$tmp/claude-list.json"
if ! jq -e '.[] | select(.id == "reef-workspaces@reef" and .enabled)' "$tmp/claude-list.json" >/dev/null; then
    fail "Claude Code does not list reef-workspaces@reef: $(cat "$tmp/claude-list.json")"
fi
workspaces=$(jq -r '.[] | select(.id == "reef-workspaces@reef") | .installPath' "$tmp/claude-list.json")
[[ -f "$workspaces/hooks/hooks.json" ]] || fail "the installed reef-workspaces plugin has no hooks"
jq -e '.[] | select(.id == "reef@reef" and .enabled and .mcpServers.reef.command == "reef")' \
    "$tmp/claude-list.json" >/dev/null || fail "Claude Code does not list reef@reef with its server: $(cat "$tmp/claude-list.json")"
installed=$(jq -r '.[] | select(.id == "reef@reef") | .installPath' "$tmp/claude-list.json")
[[ -f "$installed/skills/reef/SKILL.md" ]] || fail "the installed plugin has no skill"

step "the installed plugin's edit hook, in a workspace"
repo="$tmp/repo"
git init -q "$repo"
echo hello >"$repo/a.txt"
git -C "$repo" -c user.name=t -c user.email=t@t add -A
git -C "$repo" -c user.name=t -c user.email=t@t commit -qm first
ws=$(REEF_STATE_DIR="$state" "$bin/reef" ws create --repo "$repo" --owner test@host -o json | jq -r .path)
# The trap releases it by path.
hook=$(jq -r '.hooks.PreToolUse[0].hooks[0].command' "$installed/hooks/hooks.json")
event=$(jq -n --arg file "$ws/a.txt" --arg cwd "$ws" \
    '{session_id: "s-1", cwd: $cwd, hook_event_name: "PreToolUse", tool_name: "Edit",
      tool_input: {file_path: $file, old_string: "hello", new_string: "bye"}}')
out=$(cd "$ws" && printf '%s' "$event" | harness_env "$home" sh -c "$hook" 2>&1)
[[ -z "$out" ]] || fail "the hook printed: $out"
REEF_STATE_DIR="$state" "$bin/reef" --no-autostart presence watch --for 0 --kind editing -o json |
    jq -e 'select(.presence.by_daemon == false and .presence.paths == ["a.txt"])' >/dev/null ||
    fail "the hook declared no edit"

step "the installed plugin's prompt hook, in a workspace (R3.44)"
hook=$(jq -r '.hooks.UserPromptSubmit[0].hooks[0].command' "$installed/hooks/hooks.json")
prompt=$'add a parser test\n'
event=$(jq -n --arg cwd "$ws" --arg prompt "$prompt" \
    '{session_id: "s-1", cwd: $cwd, hook_event_name: "UserPromptSubmit", prompt: $prompt}')
out=$(cd "$ws" && printf '%s' "$event" |
    harness_env "$home" CLAUDECODE=1 CLAUDE_CODE_SESSION_ID=s-1 AI_AGENT=claude-code_2-1-316_agent \
        sh -c "$hook" 2>&1)
[[ -z "$out" ]] || fail "the prompt hook printed: $out"
expected=$(printf '%s' "$prompt" | shasum -a 256 | cut -d' ' -f1)
REEF_STATE_DIR="$state" "$bin/reef" --no-autostart ws inspect "$ws" -o json |
    jq -e --arg sha "$expected" '.provenance.context |
        select(.instruction_sha256 == $sha and .client_session == "s-1" and .harness == "claude-code/2.1.316")' \
        >/dev/null || fail "the prompt hook declared no instruction"
# With no daemon: the binary itself, without the plugin's guard, starts
# nothing, prints nothing and exits 0.
declare_hook=$(jq -r '.hooks.UserPromptSubmit[0].hooks[0].command' "$installed/hooks/hooks.json" |
    sed -e 's/^command -v reef >\/dev\/null 2>&1 && //' -e 's/ >\/dev\/null 2>&1 || true$//')
[[ "$declare_hook" == "reef provenance declare --hook "* ]] || fail "unexpected prompt hook: $declare_hook"
out=$(cd "$ws" && printf '%s' "$event" |
    harness_env "$home" REEF_STATE_DIR="$tmp/no-daemon" sh -c "$declare_hook" 2>&1) ||
    fail "the prompt hook failed with no daemon"
[[ -z "$out" ]] || fail "the prompt hook printed with no daemon: $out"
[[ ! -S "$tmp/no-daemon/daemon.sock" ]] || fail "the prompt hook started a daemon"

# Recorded, not asserted: with both, Claude Code lists Reef's server twice
# (the project's waiting for approval), as the quickstart says. A change
# here is worth a look, not a failure.
step "Claude Code with the plugin and a project .mcp.json (informational)"
(cd "$ws" && "$bin/reef" --no-register init --agents claude >/dev/null)
if ! (cd "$ws" && harness_env "$home" "$claude" mcp list 2>&1 | grep -E 'reef'); then
    echo "claude mcp list shows no reef server"
fi

step "Codex $("$codex" --version)"
# shellcheck disable=SC2016 # a jq variable, not the shell's
market=$(marketplace_copy .agents/plugins/marketplace.json '{"source": "local", "path": $path}')
home="$tmp/codex-home"
# Codex refuses a CODEX_HOME that does not exist.
mkdir -p "$home/.codex"
harness_env "$home" CODEX_HOME="$home/.codex" "$codex" plugin marketplace add "$market" --json >/dev/null
harness_env "$home" CODEX_HOME="$home/.codex" "$codex" plugin add reef@reef --json >/dev/null
harness_env "$home" CODEX_HOME="$home/.codex" "$codex" plugin list --json |
    jq -e '.installed[] | select(.pluginId == "reef@reef" and .enabled)' >/dev/null ||
    fail "Codex does not list reef@reef"
tools=$(jq -r '.tools | length' "$root/packaging/mcpb/manifest.json")
# The app server reports each MCP server it started, and its tools.
mcp_status() {
    {
        printf '%s\n' \
            '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","version":"0"}}}' \
            '{"jsonrpc":"2.0","method":"initialized"}' \
            '{"jsonrpc":"2.0","id":2,"method":"mcpServerStatus/list","params":{}}'
        sleep 15
    } | (cd "$repo" && harness_env "$1" CODEX_HOME="$1/.codex" timeout 30 "$codex" app-server 2>/dev/null) |
        jq -c 'select(.id == 2) | .result.data[] | {name, tools: (.tools | length)}'
}
status=$(mcp_status "$home")
[[ "$status" == "{\"name\":\"reef\",\"tools\":$tools}" ]] || fail "Codex's plugin server: $status, not reef with $tools tools"
(cd "$repo" && harness_env "$home" CODEX_HOME="$home/.codex" timeout 60 "$codex" debug prompt-input hi 2>/dev/null) |
    grep -qF 'reef:reef: ' || fail "the plugin's skill is not in Codex's prompt"

step "Codex through reef init --user"
home="$tmp/codex-init"
mkdir -p "$home/.codex"
HOME="$home" CODEX_HOME="$home/.codex" init --agents codex --user
harness_env "$home" CODEX_HOME="$home/.codex" "$codex" mcp list --json |
    jq -e '.[] | select(.name == "reef")' >/dev/null || fail "codex mcp list has no reef"
status=$(mcp_status "$home")
[[ "$status" == "{\"name\":\"reef\",\"tools\":$tools}" ]] || fail "Codex's configured server: $status"

step "opencode $("$opencode" --version)"
home="$tmp/opencode-home"
mkdir -p "$home/.claude/skills"
HOME="$home" XDG_CONFIG_HOME="$home/.config" init --agents opencode --user
# Claude Code's user skill too: opencode reads both directories.
HOME="$home" init --agents claude --user
config=$(cd "$tmp" && harness_env "$home" timeout 60 "$opencode" debug config)
[[ "$(jq -c '.mcp.reef' <<<"$config")" == '{"type":"local","command":["reef","mcp"],"enabled":true}' ]] ||
    fail "opencode's configuration has no reef server: $(jq -c .mcp <<<"$config")"
skills=$(cd "$tmp" && harness_env "$home" timeout 60 "$opencode" debug skill | jq '[.[] | select(.name == "reef")] | length')
[[ "$skills" -eq 1 ]] || fail "opencode lists $skills reef skills, not 1"

echo "ok: Claude Code, Codex and opencode set up from this checkout's packages"
