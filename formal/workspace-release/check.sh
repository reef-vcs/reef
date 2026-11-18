#!/usr/bin/env bash
# Model-checks WorkspaceRelease.tla with TLC: one run per property and
# scope, each outcome compared with expected.tsv. How to read and update
# the results: .claude/skills/workspace-release/SKILL.md.
#
#   check.sh                 every row of expected.tsv, on the model and,
#                            for rows expected to fail, on the model with
#                            every fix in fixes/ applied
#   check.sh PROPERTY...     only those properties
#   check.sh --mutants       also: each mutant in mutants/ must break the
#                            property it names
#
# Exits 1 if an outcome differs from expected.tsv, a mutant survives, a
# mutant or fix no longer applies, or TLC errors; 2 on bad arguments.
# Output and traces go to target/formal/workspace-release/. Uses `java` if
# one runs, else Docker with a pinned JRE image.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
out=${REEF_FORMAL_OUT:-$root/target/formal/workspace-release}

tla_version=1.7.4
tla_sha256=936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88
java_image=eclipse-temurin:21-jre@sha256:ca73b4d3961703d6d7830a4361f49ef0478f8f28b39defc07bb72f4cc7f834f6
cache=${XDG_CACHE_HOME:-$HOME/.cache}/reef-formal
jar_name=tla2tools-$tla_version.jar

mutants=false
only=()
for arg in "$@"; do
    case $arg in
        --mutants) mutants=true ;;
        -h | --help) sed -n '2,17p' "$0"; exit 0 ;;
        -*) echo "check.sh: unknown option $arg" >&2; exit 2 ;;
        *) only+=("$arg") ;;
    esac
done

if [[ ${#only[@]} -gt 0 ]]; then
    for p in "${only[@]}"; do
        if ! cut -f1 "$here/expected.tsv" | grep -qx "$p"; then
            echo "check.sh: no property $p in expected.tsv" >&2
            exit 2
        fi
    done
fi

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

mkdir -p "$cache" "$out"
if [[ ! -f $cache/$jar_name ]]; then
    curl -fsSL -o "$cache/$jar_name.part" \
        "https://github.com/tlaplus/tlaplus/releases/download/v$tla_version/tla2tools.jar"
    mv "$cache/$jar_name.part" "$cache/$jar_name"
fi
if [[ $(sha256 "$cache/$jar_name") != "$tla_sha256" ]]; then
    echo "check.sh: $cache/$jar_name does not match its pinned checksum; delete it and rerun" >&2
    exit 1
fi

if java -version >/dev/null 2>&1; then
    runner=java
elif docker version >/dev/null 2>&1; then
    runner=docker
else
    echo "check.sh: needs a Java runtime (11+) or a running Docker daemon" >&2
    exit 1
fi

# The constants of each scope: how many failures, crashes and quiesces a
# behavior may contain.
scope_constants() {
    case $1 in
        crash) echo "MaxFailures = 2 MaxCrashes = 1 MaxQuiesces = 1" ;;
        nocrash) echo "MaxFailures = 2 MaxCrashes = 0 MaxQuiesces = 1" ;;
        *) echo "check.sh: unknown scope $1" >&2; exit 2 ;;
    esac
}

# apply SPEC OUT EDITS...: writes SPEC with each EDITS file's
# <<< old === new >>> blocks applied to OUT. Every old text must occur in
# SPEC exactly once. Prints why and returns 1 if one does not.
apply() {
    local spec=$1 dest=$2
    shift 2
    perl -e '
        my ($spec, @files) = @ARGV;
        local $/; open my $s, "<", $spec or die "$spec: $!\n"; my $text = <$s>;
        for my $file (@files) {
            open my $m, "<", $file or die "$file: $!\n"; my $edits = <$m>;
            while ($edits =~ /^<<<\n(.*?)^===\n(.*?)^>>>\n/msg) {
                my ($old, $new) = ($1, $2);
                my $count = () = $text =~ /\Q$old\E/g;
                if ($count != 1) {
                    my ($first) = $old =~ /^\s*(.*)$/m;
                    die "$file: an old text occurs $count times, not once: $first\n";
                }
                $text =~ s/\Q$old\E/$new/;
            }
        }
        print $text;
    ' "$spec" "$@" >"$dest"
}

# tlc DIR NAME KIND PROPERTY SCOPE: model-checks DIR/WorkspaceRelease.tla
# and prints pass, fail or error. Leaves DIR/NAME.out and, on a fail,
# DIR/NAME.trace.
tlc() {
    local dir=$1 name=$2 kind=$3 property=$4 scope=$5
    rm -f "$dir/$name.trace"
    printf 'SPECIFICATION Spec\nCONSTANTS %s\n%s %s\n' \
        "$(scope_constants "$scope")" "$kind" "$property" >"$dir/$name.cfg"
    local args=(-XX:+UseParallelGC -cp "$jar_name" tlc2.TLC -deadlock -workers auto
        -metadir "meta-$name" -config "$name.cfg" WorkspaceRelease.tla)
    cp "$cache/$jar_name" "$dir/$jar_name"
    if [[ $runner == java ]]; then
        (cd "$dir" && java "${args[@]}") >"$dir/$name.out" 2>&1 || true
    else
        docker run --rm -v "$dir:/w" -w /w "$java_image" java "${args[@]}" \
            >"$dir/$name.out" 2>&1 || true
    fi
    rm -rf "${dir:?}/meta-$name"
    if grep -q 'No error has been found' "$dir/$name.out"; then
        echo pass
    elif grep -qE 'is violated|Temporal properties were violated' "$dir/$name.out"; then
        awk -f "$here/trace.awk" "$dir/$name.out" >"$dir/$name.trace"
        echo fail
    else
        echo error
    fi
}

wanted() {
    [[ ${#only[@]} -eq 0 ]] && return 0
    local p
    for p in "${only[@]}"; do [[ $p == "$1" ]] && return 0; done
    return 1
}

status=0
rows=$(grep -v '^#' "$here/expected.tsv" | grep -v '^$')

echo "On the model:"
cp "$here/WorkspaceRelease.tla" "$out/"
printf '%-24s %-8s %-9s %-8s %s\n' property scope expected outcome finding
while IFS=$'\t' read -r property kind scope expected _fixed finding; do
    wanted "$property" || continue
    outcome=$(tlc "$out" "$property.$scope" "$kind" "$property" "$scope")
    mark=""
    if [[ $outcome != "$expected" ]]; then
        mark="  <- differs: $out/$property.$scope.out"
        status=1
    fi
    printf '%-24s %-8s %-9s %-8s %s%s\n' "$property" "$scope" "$expected" "$outcome" "$finding" "$mark"
done <<<"$rows"

# A row that fails on a known finding would hide a new violation of the
# same property; with every fix applied, it shows.
echo
echo "With every fix in fixes/ applied:"
fixed_dir=$out/fixed
mkdir -p "$fixed_dir"
if ! apply "$here/WorkspaceRelease.tla" "$fixed_dir/WorkspaceRelease.tla" "$here"/fixes/*.fix; then
    echo "  a fix no longer applies; update it (see the skill)"
    status=1
else
    printf '%-24s %-8s %-9s %s\n' property scope expected outcome
    while IFS=$'\t' read -r property kind scope expected fixed _finding; do
        wanted "$property" || continue
        [[ $expected == fail ]] || continue
        outcome=$(tlc "$fixed_dir" "$property.$scope" "$kind" "$property" "$scope")
        mark=""
        if [[ $outcome != "$fixed" ]]; then
            mark="  <- differs: $fixed_dir/$property.$scope.out"
            status=1
        fi
        printf '%-24s %-8s %-9s %s%s\n' "$property" "$scope" "$fixed" "$outcome" "$mark"
    done <<<"$rows"
fi

if $mutants; then
    echo
    printf '%-44s %-24s %s\n' mutant breaks outcome
    for file in "$here"/mutants/*.mutant; do
        name=$(basename "$file" .mutant)
        read -r property kind scope < <(sed -n 's/^# breaks: //p' "$file")
        wanted "$property" || continue
        dir=$out/mutants/$name
        mkdir -p "$dir"
        # On a property that already fails on a known finding, a failure
        # proves nothing: break the fixed model instead.
        base_fails=$(awk -F'\t' -v p="$property" -v s="$scope" \
            '$1 == p && $3 == s && $4 == "fail" { print "yes" }' "$here/expected.tsv")
        edits=("$file")
        if [[ $base_fails == yes ]]; then
            edits=("$here"/fixes/*.fix "$file")
            name="$name (on fixed)"
        fi
        if ! apply "$here/WorkspaceRelease.tla" "$dir/WorkspaceRelease.tla" "${edits[@]}" 2>"$dir/apply.err"; then
            printf '%-44s %-24s %s\n' "$name" "$property" "stale  <- $(cat "$dir/apply.err")"
            status=1
            continue
        fi
        outcome=$(tlc "$dir" "$property.$scope" "$kind" "$property" "$scope")
        mark=""
        if [[ $outcome != fail ]]; then
            mark="  <- survived: $dir/$property.$scope.out"
            status=1
        fi
        printf '%-44s %-24s %s%s\n' "$name" "$property" "$outcome" "$mark"
    done
fi

echo
echo "traces: $out/*.trace and $fixed_dir/*.trace"
exit $status
