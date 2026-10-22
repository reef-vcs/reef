#!/bin/sh
# Installs Reef's prebuilt `reef` and `reef-daemon` binaries (R1.28).
#
#   curl -fsSL https://raw.githubusercontent.com/reef-vcs/reef/main/install.sh | sh
#
# Never prompts and never uses sudo. Settings, all optional, from the
# environment:
#
#   REEF_VERSION      the release to install, `0.1.0` or `v0.1.0`; default the
#                     latest. Pin it where many machines share an IP (CI).
#   REEF_INSTALL_DIR  where the binaries go; default ~/.local/bin.
#   REEF_INSTALL_URL  a base URL holding the release's tarballs and SHA256SUMS,
#                     in place of the GitHub Release (a mirror, or file:// in CI).
#   REEF_TARGET       the target triple to install, in place of the detected one.
#
# Each tarball is checked against the release's SHA256SUMS before anything is
# installed. Exits 0, or 1 with a one-line reason on stderr.

set -eu

REPO_URL=https://github.com/reef-vcs/reef

say() {
    printf 'reef install: %s\n' "$*" >&2
}

die() {
    say "error: $*"
    exit 1
}

has() {
    command -v "$1" >/dev/null 2>&1
}

# download URL FILE
download() {
    if has curl; then
        # -f without -S: the one line on failure is ours, with curl's code.
        curl --proto '=https,file' --proto-redir '=https' --tlsv1.2 -fsL "$1" -o "$2" ||
            die "could not download $1 (curl exit $?)"
    elif has wget; then
        case "$1" in
            https://*) ;;
            *) die "wget cannot fetch $1; install curl" ;;
        esac
        wget -q "$1" -O "$2" || die "could not download $1 (wget exit $?)"
    else
        die "neither curl nor wget is installed"
    fi
}

# sha256 FILE: the file's SHA-256 in hex
sha256() {
    if has sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif has shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif has openssl; then
        openssl dgst -sha256 -r "$1" | cut -d' ' -f1
    else
        die "no sha256sum, shasum, or openssl to verify the download with"
    fi
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Darwin)
            os=apple-darwin
            # A shell under Rosetta reports x86_64 on Apple silicon.
            if [ "$arch" = x86_64 ] && [ "$(/usr/sbin/sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
                arch=arm64
            fi
            ;;
        Linux) os=unknown-linux-musl ;;
        *) die "no Reef binaries for $os; build from source: cargo install --git $REPO_URL reef-vcs" ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        arm64 | aarch64) arch=aarch64 ;;
        *) die "no Reef binaries for $arch; build from source: cargo install --git $REPO_URL reef-vcs" ;;
    esac
    echo "$arch-$os"
}

main() {
    if [ $# -gt 0 ]; then
        die "takes no arguments; set REEF_VERSION, REEF_INSTALL_DIR, REEF_INSTALL_URL, or REEF_TARGET"
    fi
    has uname || die "uname is not installed"
    has tar || die "tar is not installed"

    if [ -n "${REEF_TARGET:-}" ]; then
        target=$REEF_TARGET
    else
        target=$(detect_target) || exit 1
    fi
    version=${REEF_VERSION:-}
    version=${version#v}
    if [ -n "${REEF_INSTALL_URL:-}" ]; then
        base=${REEF_INSTALL_URL%/}
    elif [ -n "$version" ]; then
        base=$REPO_URL/releases/download/v$version
    else
        # GitHub redirects this to the latest release's asset: no API call.
        base=$REPO_URL/releases/latest/download
    fi
    if [ -n "${REEF_INSTALL_DIR:-}" ]; then
        dir=$REEF_INSTALL_DIR
    else
        [ -n "${HOME:-}" ] || die "HOME is not set; set REEF_INSTALL_DIR"
        dir=$HOME/.local/bin
    fi

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t reef-install) || die "could not create a temporary directory"
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 1' HUP INT TERM

    download "$base/SHA256SUMS" "$tmp/SHA256SUMS"
    # Lines are `<sha256>  reef-<version>-<target>.tar.gz`; print the one
    # for this target (and version, if pinned) as `<sha256> <name>`.
    line=$(awk -v target="$target" -v version="$version" '
        {
            name = $2
            sub(/^\*/, "", name)
            suffix = "-" target ".tar.gz"
            if (version != "") {
                if (name == "reef-" version suffix) print $1, name
            } else if (substr(name, 1, 5) == "reef-" && length(name) > length(suffix) + 5 &&
                       substr(name, length(name) - length(suffix) + 1) == suffix) {
                print $1, name
            }
        }' "$tmp/SHA256SUMS")
    [ -n "$line" ] || die "no Reef ${version:-release} for $target at $base"
    [ "$(printf '%s\n' "$line" | wc -l)" -eq 1 ] || die "SHA256SUMS at $base lists more than one tarball for $target"
    expected=${line%% *}
    tarball=${line#* }
    version=${tarball#reef-}
    version=${version%-"$target".tar.gz}
    case "$version" in
        "" | *[!0-9A-Za-z.+-]*) die "SHA256SUMS at $base names an unexpected tarball: $tarball" ;;
    esac
    if [ "$base" = "$REPO_URL/releases/latest/download" ]; then
        # Pin the release SHA256SUMS came from, in case a newer one lands now.
        base=$REPO_URL/releases/download/v$version
    fi

    say "downloading reef $version for $target"
    download "$base/$tarball" "$tmp/$tarball"
    actual=$(sha256 "$tmp/$tarball")
    [ "$actual" = "$expected" ] || die "checksum mismatch for $tarball: expected $expected, got $actual"

    tar -xzf "$tmp/$tarball" -C "$tmp" || die "could not unpack $tarball"
    unpacked=$tmp/reef-$version-$target
    mkdir -p "$dir" || die "could not create $dir"
    for bin in reef reef-daemon; do
        [ -f "$unpacked/$bin" ] || die "$tarball has no $bin"
        # Copy beside, then rename over: a running binary cannot be
        # overwritten in place on Linux, but it can be replaced.
        partial=$dir/.$bin.$$
        if ! { cp "$unpacked/$bin" "$partial" && chmod 755 "$partial" && mv -f "$partial" "$dir/$bin"; }; then
            rm -f "$partial"
            die "could not write $dir/$bin"
        fi
    done

    installed=$("$dir/reef" --version 2>/dev/null) || die "$dir/reef does not run on this machine"
    say "installed $installed in $dir"
    case ":${PATH:-}:" in
        *":$dir:"*) ;;
        *)
            # Literal `$HOME` and `$PATH`: the line is for the profile.
            # shellcheck disable=SC2016
            if [ -z "${REEF_INSTALL_DIR:-}" ]; then
                shown='$HOME/.local/bin'
            else
                shown=$dir
            fi
            say "$dir is not on PATH; add this line to your shell's profile:"
            # shellcheck disable=SC2016
            printf '  export PATH="%s:$PATH"\n' "$shown" >&2
            ;;
    esac
}

main "$@"
