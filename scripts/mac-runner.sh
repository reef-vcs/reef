#!/usr/bin/env bash
# Installs or removes the self-hosted macOS runner that CI's macOS legs use when the repository
# variable MACOS_RUNNER is `self-hosted` (see the `plan` job in .github/workflows/ci.yml).
#
#   sudo scripts/mac-runner.sh install [TOKEN]   TOKEN from: gh api -X POST repos/reef-vcs/reef/actions/runners/registration-token --jq .token
#   sudo scripts/mac-runner.sh uninstall TOKEN   TOKEN from: gh api -X POST repos/reef-vcs/reef/actions/runners/remove-token --jq .token
#
# The runner runs from a LaunchDaemon as `reefci`, a hidden system-range user with its own group
# and no password: no login session is needed, and it is outside `staff`, so jobs cannot read the
# owner's home, keychain, SSH keys, toolchains or Reef daemon. The repository is public. ci.yml
# sends pull requests from forks, and Dependabot's, to GitHub's runners, and since a pull
# request's workflow file comes from its own branch, the job hook (mac-runner-hook.sh) refuses
# them here too. Root writes only outside reefci's home (the hook, the LaunchDaemon, the log), so
# a job cannot redirect those writes. Running `install` again (no token needed once registered)
# updates the hook and the LaunchDaemon, stopping a job that is running; the runner updates
# itself. `uninstall` keeps the user and its home (target/ and toolchains):
# `sysadminctl -deleteUser reefci` and `dscl . delete /Groups/reefci` remove them.
set -euo pipefail

user=reefci
home=/Users/$user
dir=$home/actions-runner
label=dev.reef.actions-runner
plist=/Library/LaunchDaemons/$label.plist
hooks=/usr/local/libexec/reef-runner
log=/var/log/reef-runner.log
repo_url=https://github.com/reef-vcs/reef
# actions/runner's release; its notes list the sha256 of each tarball.
version=2.338.2
sha256=25a065af409ffa317ab4d5a3b614dd15fc9c40f2172bc1b35a1b1258e919a388

here=$(cd "$(dirname "$0")" && pwd)

die() { echo "mac-runner.sh: $*" >&2; exit 1; }
as_user() { (cd "$home" && sudo -u "$user" -H "$@"); }
stop_runner() {
    launchctl bootout "system/$label" 2>/dev/null || true
    # bootout returns before the job is gone, and an early bootstrap fails with an I/O error.
    for _ in $(seq 30); do
        launchctl print "system/$label" >/dev/null 2>&1 || break
        sleep 1
    done
    pkill -9 -u "$user" 2>/dev/null || true
}

case "$*" in
    install | "install "* | "uninstall "?*) ;;
    *) die "usage: sudo $0 install [TOKEN] | uninstall TOKEN" ;;
esac
[[ $# -le 2 ]] || die "usage: sudo $0 install [TOKEN] | uninstall TOKEN"
[[ $(id -u) -eq 0 ]] || die "run with sudo: it creates a user and a LaunchDaemon"
[[ $(uname -sm) == "Darwin arm64" ]] || die "for an Apple silicon Mac only"
if ! xcrun --find cc >/dev/null 2>&1; then
    die "install the Xcode command line tools first: xcode-select --install"
fi
token=${2:-}

if [[ $1 == uninstall ]]; then
    stop_runner
    rm -f "$plist"
    if [[ -f $dir/.runner ]]; then
        (cd "$dir" && sudo -u "$user" -H ./config.sh remove --token "$token")
    fi
    echo "runner removed; the $user user and $home remain"
    exit 0
fi

if ! id "$user" >/dev/null 2>&1; then
    # A system-range id that is free as both a user and a group id.
    taken=$( (dscl . list /Users UniqueID; dscl . list /Groups PrimaryGroupID) | awk '{ print $2 }')
    id=""
    for candidate in $(seq 450 499); do
        if ! grep -qx "$candidate" <<<"$taken"; then id=$candidate; break; fi
    done
    [[ -n $id ]] || die "no free id between 450 and 499"
    dscl . create "/Groups/$user"
    dscl . create "/Groups/$user" PrimaryGroupID "$id"
    dscl . create "/Groups/$user" RealName "Reef CI"
    dscl . create "/Groups/$user" Password '*'
    dscl . create "/Users/$user"
    dscl . create "/Users/$user" UniqueID "$id"
    dscl . create "/Users/$user" PrimaryGroupID "$id"
    dscl . create "/Users/$user" RealName "Reef CI"
    dscl . create "/Users/$user" NFSHomeDirectory "$home"
    dscl . create "/Users/$user" UserShell /bin/zsh
    dscl . create "/Users/$user" Password '*'
    dscl . create "/Users/$user" IsHidden 1
    id "$user" >/dev/null 2>&1 || die "dscl did not create $user"
fi
if dsmemberutil checkmembership -U "$user" -G staff | grep -q "is a member"; then
    die "$user is in staff, so it could read other users' homes; give it a group of its own"
fi
mkdir -p "$home"
chown "$user:$user" "$home"
chmod 0700 "$home"

stop_runner

if [[ ! -x $home/.cargo/bin/rustup ]]; then
    # No toolchain: each job installs the one rust-toolchain.toml pins (`rustup toolchain install`).
    as_user /bin/sh -c 'curl --proto =https --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none'
fi

as_user mkdir -p "$dir" "$home/tmp"
if [[ ! -x $dir/run.sh ]]; then
    tarball=$dir/actions-runner-$version.tar.gz
    as_user curl -fsSL --retry 3 -o "$tarball" \
        "https://github.com/actions/runner/releases/download/v$version/actions-runner-osx-arm64-$version.tar.gz"
    echo "$sha256  $tarball" | shasum -a 256 -c - >/dev/null || die "checksum mismatch for $tarball"
    as_user tar -xzf "$tarball" -C "$dir"
    rm -f "$tarball"
fi

if [[ ! -f $dir/.runner ]]; then
    [[ -n $token ]] || die "the runner is not registered yet: pass a registration token"
    # A fixed name: it is printed in every job's public log.
    (cd "$dir" && sudo -u "$user" -H ./config.sh --unattended --url "$repo_url" --token "$token" \
        --name reef-mac --labels reef-mac --work _work --replace)
fi
# config.sh writes .env and .path from its own environment, and the runner would prefer them to
# the LaunchDaemon's; the job's environment, hooks included, comes from the LaunchDaemon only.
rm -f "$dir/.env" "$dir/.path"

install -d -o root -g wheel -m 0755 "$hooks"
install -o root -g wheel -m 0755 "$here/mac-runner-hook.sh" "$hooks/job-started.sh"
install -o root -g wheel -m 0755 "$here/mac-runner-hook.sh" "$hooks/job-completed.sh"
touch "$log"
chown "$user:$user" "$log"
chmod 0640 "$log"

# PATH leaves out Homebrew and /usr/local/bin, so jobs see the system's tools and their own.
# ProcessType Interactive: launchd would otherwise run a daemon at background priority, and the
# timing-sensitive tests would see a slower machine than the developer's. launchd's default soft
# limit of 256 open files is far below what the daemon tests open at once.
cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$label</string>
    <key>UserName</key><string>$user</string>
    <key>GroupName</key><string>$user</string>
    <key>ProgramArguments</key><array><string>$dir/run.sh</string></array>
    <key>WorkingDirectory</key><string>$dir</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key><string>$home</string>
        <key>USER</key><string>$user</string>
        <key>LOGNAME</key><string>$user</string>
        <key>TMPDIR</key><string>$home/tmp/</string>
        <key>LANG</key><string>en_US.UTF-8</string>
        <key>PATH</key><string>$home/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
        <key>ACTIONS_RUNNER_HOOK_JOB_STARTED</key><string>$hooks/job-started.sh</string>
        <key>ACTIONS_RUNNER_HOOK_JOB_COMPLETED</key><string>$hooks/job-completed.sh</string>
    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Interactive</string>
    <key>SoftResourceLimits</key><dict><key>NumberOfFiles</key><integer>65536</integer></dict>
    <key>HardResourceLimits</key><dict><key>NumberOfFiles</key><integer>65536</integer></dict>
    <key>StandardOutPath</key><string>$log</string>
    <key>StandardErrorPath</key><string>$log</string>
</dict>
</plist>
EOF
chown root:wheel "$plist"
chmod 0644 "$plist"
launchctl bootstrap system "$plist"
echo "runner started; log: $log, diagnostics: $dir/_diag"
echo "send macOS jobs here: gh variable set MACOS_RUNNER --body self-hosted --repo reef-vcs/reef"
