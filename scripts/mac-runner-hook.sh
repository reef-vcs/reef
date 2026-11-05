#!/usr/bin/env bash
# The self-hosted macOS runner's job hooks, installed root-owned by mac-runner.sh as
# job-started.sh (ACTIONS_RUNNER_HOOK_JOB_STARTED) and job-completed.sh (..._COMPLETED), outside
# the runner's directory, so a job cannot change them. They run as reefci.
#
# The runner runs a hook as `bash -e`, and a started hook that exits non-zero fails the job before
# it runs: hence `set +e`, and only the event check below exits 1. The runner is persistent and
# takes one job at a time, so everything of reefci's under _work and in its scratch directories is
# a previous job's leftovers; target/ stays, warm, which is the point of the runner.
set +e

repo=reef-vcs/reef
work=$HOME/actions-runner/_work
uid=$(id -u)
# Free space below which the build caches go.
min_free_gb=50

if [[ $(basename "$0") == job-started.sh ]]; then
    # ci.yml's `plan` job sends only these here, but a pull request's workflow file comes from its
    # own branch, so a fork could name this runner's labels: the runner's side checks too.
    allowed=false
    workflow_ok=false
    [[ ${GITHUB_WORKFLOW_REF:-} == "$repo/.github/workflows/ci.yml@"* ]] && workflow_ok=true
    case "${GITHUB_EVENT_NAME:-}" in
        push | workflow_dispatch)
            [[ ${GITHUB_REPOSITORY:-} == "$repo" ]] && allowed=true
            ;;
        pull_request)
            head=$(/usr/bin/jq -r '.pull_request.head.repo.full_name // ""' "${GITHUB_EVENT_PATH:-/dev/null}")
            author=$(/usr/bin/jq -r '.pull_request.user.login // ""' "${GITHUB_EVENT_PATH:-/dev/null}")
            [[ $head == "$repo" && $author != "dependabot[bot]" ]] && allowed=true
            ;;
    esac
    [[ ${GITHUB_ACTOR:-} == "dependabot[bot]" ]] && allowed=false
    if [[ $allowed != true || $workflow_ok != true ]]; then
        echo "::error::this runner takes $repo's ci.yml on pushes, manual runs and its own branches' pull requests, not ${GITHUB_EVENT_NAME:-?} from ${head:-${GITHUB_REPOSITORY:-?}} by ${GITHUB_ACTOR:-?} (${GITHUB_WORKFLOW_REF:-?})"
        exit 1
    fi
fi

sweep() {
    chmod -R u+rwx "$@" 2>/dev/null
    rm -rf "$@" 2>/dev/null
}

# Everything below acts on all of the user's processes and files: reefci's only.
if [[ $(id -un) != reefci ]]; then
    echo "mac-runner-hook.sh: not reefci; nothing cleaned" >&2
    exit 0
fi

# Every process of reefci's but this hook and the runner it runs under (its ancestors: the worker,
# the listener, run.sh), so nothing a job started, under any name, outlives it. The list is read
# first and the kills are builtins, so no process of the hook's own is in it when they run.
keep=" $$ "
pid=$$
while :; do
    pid=$(ps -o ppid= -p "$pid" | tr -d ' ')
    [[ -n $pid && $pid -gt 1 ]] || break
    keep+="$pid "
done
mine=$(ps -axo pid=,uid= | awk -v uid="$uid" '$2 == uid { print $1 }')
for pid in $mine; do
    [[ $keep == *" $pid "* ]] || kill -9 "$pid" 2>/dev/null
done

# Scratch directories, including tests' whose cleanup a kill skipped (some are mode 000). The
# daemon tests put theirs in /tmp, for short socket paths. .NET's own sockets in TMPDIR stay.
while IFS= read -r -d '' entry; do
    sweep "$entry"
done < <(
    find "$HOME/tmp" -mindepth 1 -maxdepth 1 ! -name 'dotnet-diagnostic-*' ! -name 'clr-debug-pipe-*' -print0 2>/dev/null
    find /private/tmp -mindepth 1 -maxdepth 1 -user "$uid" -print0 2>/dev/null
)

# ci.yml checks out with `clean: false` here so that target/ survives; everything else
# untracked goes, so a file from another branch (a stray tests/*.rs) cannot reach the build.
for checkout in "$work"/*/*/; do
    [[ -d $checkout.git ]] && git -C "$checkout" clean -ffdxq -e /target/ 2>/dev/null
done

free_gb() { df -g "$HOME" | awk 'NR == 2 { print $4 }'; }
before=$(free_gb)
if (( before < min_free_gb )); then
    sweep "$work"/*/*/target "$work/_tool" "$HOME/Library/Caches" "$HOME/.npm/_cacache"
    # Toolchains other than the ones the checkouts pin.
    keep=$(cat "$work"/*/*/rust-toolchain.toml 2>/dev/null | sed -n 's/^channel *= *"\(.*\)"/\1/p' | sort -u)
    rustup toolchain list 2>/dev/null | awk '{ print $1 }' | while read -r toolchain; do
        grep -qx "${toolchain%%-*}" <<<"$keep" || rustup toolchain uninstall "$toolchain" >/dev/null 2>&1
    done
    echo "::warning::${before} GB was free on the runner's disk, now $(free_gb) GB: removed every target/, the tool cache, caches and unpinned toolchains, so this build is cold"
fi

if [[ $(basename "$0") == job-started.sh ]]; then
    # The sandbox's canary: a port it blocks that answers means the runner is running outside it.
    for port in $(grep -o 'localhost:[0-9]*' "$(dirname "$0")/runner.sb" 2>/dev/null | cut -d: -f2); do
        if /usr/bin/nc -z -G 1 127.0.0.1 "$port" >/dev/null 2>&1; then
            echo "::warning::localhost:$port answers inside the runner, so its sandbox is off: run scripts/mac-runner.sh install again"
        fi
    done
    # Keep the Mac awake for the job, on battery too; caffeinate ends with the job's worker.
    worker=$(pgrep -u "$uid" -x Runner.Worker | head -1)
    [[ -n $worker ]] && /usr/bin/caffeinate -i -w "$worker" >/dev/null 2>&1 &
fi
exit 0
