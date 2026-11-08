# The reference executor

`reef executor` runs checks for Reef. It claims jobs from a daemon, reads each job's tree and checks it against the tree id, runs the commands declared for the requested kinds and targets in a sandbox, and returns signed evidence. It speaks [executor protocol 1](executor-protocol.md); the decisions behind it are [ADR 0060](adr/0060-reference-executor.md).

Agents don't run executors; an operator does: on a laptop, on a build machine, or in a CI job. Both commands are in `reef --help-all`, not the root help.

```sh
reef executor check                    # the configuration, and the sandbox's escape probes
reef executor check -r @ --kind test   # what this revision's tree would run, without running it
reef executor run                      # claim and run jobs until stopped (Ctrl-C releases them)
```

Then, from anywhere that reaches the same daemon:

```sh
reef job submit --kind lint --wait
reef job submit --kind test --target crates/reef-evidence --wait
reef job show <id>
reef evidence list --job <id>
```

## Running it locally

On a laptop the executor talks to the local daemon over its socket and signs with session keys bound to the default person identity. With no configuration it runs the checks the tree's `reef-checks.toml` declares, for the kinds `build`, `lint` and `test`, in this platform's sandbox. It refuses to start if the sandbox can't be applied (exit 6); `reef executor check` says why.

`reef executor run` serves until stopped. It prints a line per finished job on stderr (`job job_8f3a… lint test: done  pass 3 fail 0 error 0  42s`), or, with `-o json` or when piped, one JSON object per job on stdout ([`reef schema cli executor run`](agent-guide.md)). The first `SIGINT` or `SIGTERM` stops claiming, kills running checks, sends their finished pairs' evidence and gives the jobs back (`UpdateJob` with `release`). A second signal within five seconds exits at once and leaves the leases to lapse.

| Flag | |
| --- | --- |
| `--config FILE` | the configuration; default `REEF_EXECUTOR_CONFIG`, else `executor.toml` in the client's state directory, else the defaults |
| `--repo ID` | only jobs of this repository (repeatable), added to `[executor] repos` |
| `--jobs N` | run N jobs at once, 1 to 16 (each in its own directory and session) |
| `--once [--wait 60s] [--require-job]` | claim one job (one per `--jobs`), run it, exit; `--require-job` exits 3 if none came |
| `--exit-when-idle 2m` | exit 0 after this long with no job: a CI step that drains the queue |
| `--keep-work` | keep each job's directory, for debugging |
| `--token-file FILE` | the remote token, instead of `REEF_REMOTE_TOKEN` |

## Running it in CI

In CI the executor dials the daemon's [remote listener](adr/0049-remote-listener.md) with a token linked to a `ci` identity, so its evidence is bound to that identity and policy can trust it (R3.07). On the daemon's machine:

```sh
reef identity create ci-main --kind ci
reef remote token create ci-runner --scope write --identity ci-main --expires 90d
```

Store the printed token as a CI secret, and the daemon's fingerprint (`reef remote show`) beside it. A GitHub Actions job:

```yaml
jobs:
  checks:
    runs-on: ubuntu-24.04
    steps:
      - name: Install Reef and bubblewrap
        run: |
          curl -fsSL https://raw.githubusercontent.com/reef-vcs/reef/main/install.sh | sh
          sudo apt-get install -y bubblewrap
      - name: Let bubblewrap create user namespaces
        run: sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
      # The token reaches a file in its own step, so the step that runs the
      # executor (and its shell, the executor's parent) never holds it.
      - name: Write the token
        env:
          TOKEN: ${{ secrets.REEF_CI_TOKEN }}
        run: printf '%s\n' "$TOKEN" > "$RUNNER_TEMP/reef-token"
      - name: Run the queued checks
        env:
          REEF_REMOTE: https://reef.example.com:7455
          REEF_REMOTE_FINGERPRINT: ${{ secrets.REEF_FINGERPRINT }}
        run: reef executor run --token-file "$RUNNER_TEMP/reef-token" --exit-when-idle 2m
```

`--token-file` keeps the token out of every process's environment; the file is hidden from every check. With `REEF_REMOTE_TOKEN` set instead, the executor on macOS runs itself again with only the variables it needs and the token on a pipe, because a check there can read another process's environment (see "The sandbox"); its parent's environment (the shell that started it) still holds the variable, which is why the file is the way. On a macOS CI runner, assume a check can read the environments of the runner's processes (the runner agent's own tokens among them): a CI identity whose evidence must be strict runs its executor on Linux.

A trusted CI identity should run each job on a fresh VM or container: the sandbox narrows what a tree's code can reach, but it is not a VM.

## Declaring checks

A **check** is one declared command for one evidence kind. A job's pairs (each requested kind with each target, or each kind alone) are matched to checks, and each matched check runs as one sandboxed process.

```toml
[[check]]
name = "crate"                    # unique in its file: a-z 0-9 _ -, at most 64
kind = "test"                     # the evidence kind it produces
targets = ["crates/*"]            # globs over the target names it accepts
run = ["cargo", "test", "--workspace", "--offline"]            # for the whole check
run_target = ["cargo", "test", "--offline", "--manifest-path", "{target}/Cargo.toml"]
timeout = "20m"                   # at most the job's remaining wall time, the default
network = "none"                  # none | loopback | full
env = { RUST_BACKTRACE = "1" }    # literal values
artifacts = ["target/nextest/default/junit.xml"]   # tree-relative files uploaded after the run
error_exit = [75]                 # exit codes that mean "no verdict"
```

Checks come from two places:

- **`executor.toml`**, the executor's own `[[check]]` tables. These may also set `cache = true` and any network level.
- **`reef-checks.toml`** at the root of the job's tree, unless the executor says `[repository] checks = "deny"`. A repository may declare everything above, but its `network` is lowered to `[sandbox] max_repository_network` (default `none`, and the object's note says so). It may not ask for a cache, pass variables through from the executor's environment, name host paths, or choose the sandbox: those are what separate a run from the host and from other trees. The file is parsed strictly; a file that does not parse, or declares more than 64 checks, makes each pair it would have served `error` evidence naming the line, not a failed job.

For each pair, the executor's own checks of that kind are tried first (with a target: those whose `targets` match it and that have `run_target`; for the whole check: those with `run`). Only when none matches are the tree's tried the same way. Several matching checks all run, the executor's first, in declaration order. All of a job's pairs and checks run in the same directory, one after another, so a later check sees what earlier ones wrote, as in a CI job: every check runs the tree's own code either way. A pair nothing matches gets `error` evidence: "no check for kind build as a whole in this executor's configuration, and the tree has no reef-checks.toml".

Why a repository may declare at all: whatever runs on a tree is the tree's code. `cargo test` runs the tree's build scripts and tests; `just test` runs its `justfile`. A declaration in the tree changes which entry point runs, not whose code runs. A change to `reef-checks.toml` still changes how a tree is judged, so it shows in the proposal's diff, and every object it produced records the file's SHA-256 (`repository_checks`) and each check's declaration digest. Policy (R3.07) should treat the file as sign-off material.

The kinds an executor claims are its own checks' kinds, plus `[repository] kinds` (default `build`, `lint`, `test`) when repository checks are allowed. It can't see a tree's declarations before it claims a job on it, so a claimed kind the tree turns out not to declare gets `error` evidence.

### Targets are data

A requested target reaches an argv only when it matches the check's `targets` globs (`*` does not cross `/`, `**` does) and the executor's target pattern, `^[A-Za-z0-9._/:@+=-]{1,512}$` by default (`[declarations] target_pattern`), with no `..` component, and not starting with `-` or `/`. So no target with spaces, quotes, `$`, `;`, backticks or newlines, no option and no absolute path ever reaches a program. `{target}` may stand for a whole argv element or part of one (`--package={target}`); `{{` and `}}` are literal braces.

There is never a shell between the executor and the program. A declaration that puts `{target}` in a shell's `-c` script (`["sh", "-c", "cargo test -p {target}"]`) is refused when the file is read. Pass the target as an argument after the script instead:

```toml
run_target = ["sh", "-c", "cargo test -p \"$1\"", "sh", "{target}"]
```

### What a check sees

The working directory is the job's tree, writable. The environment is built from nothing:

| Variable | |
| --- | --- |
| `PATH` | the executor's (toolchains are found only where `[sandbox] read` exposes them) |
| `HOME`, `TMPDIR` | the job's own, empty at start |
| `TERM=dumb`, and `LANG`, `LC_ALL`, `TZ` | when the executor has them |
| `REEF_CHECK_JOB`, `REEF_CHECK_KIND`, `REEF_CHECK_TARGET`, `REEF_CHECK_TREE`, `REEF_CHECK_NAME` | what is running |
| `REEF_CHECK_CACHE` | the check's cache, when it has one |
| `[sandbox] pass_env` | copied from the executor's environment (default `RUSTUP_HOME`, `CARGO_HOME`); never a `REEF_` name |
| `[sandbox] env` | the executor's literal variables for every check (default `RUSTUP_HOME`, `CARGO_HOME` at `~/.rustup` and `~/.cargo`) |
| the check's `env` | literal; `{cache}` in a value is the cache directory; a repository may not set a loader variable (`LD_*`, `DYLD_*`) |

Nothing else crosses: no `REEF_REMOTE_TOKEN`, `REEF_SOCKET`, `GITHUB_TOKEN`, `ACTIONS_ID_TOKEN_REQUEST_TOKEN` or `SSH_AUTH_SOCK`.

### Caches

Without a cache every build starts cold. An executor check with `cache = true` gets a persistent directory, `<work>/caches/<repo>/<check>/`, writable inside the sandbox, named by `REEF_CHECK_CACHE` and by `{cache}` in its `env`:

```toml
[[check]]
name = "clippy"
kind = "lint"
run = ["cargo", "clippy", "--workspace", "--all-targets", "--offline", "--", "-D", "warnings"]
cache = true
env = { CARGO_TARGET_DIR = "{cache}/target" }
```

A cache lets one tree's run leave files that change another tree's result (a poisoned build output). So caches are off by default, only the executor's own checks can have them, and every object a cached check produced says `cache: true` in its annotations. An operator whose CI identity must be strict leaves them off.

## Results

| How the check ended | Its result |
| --- | --- |
| exit 0 | `pass` |
| an exit code in `error_exit` | `error` |
| another exit code, or a signal the executor did not send (the tree's code crashed) | `fail` |
| stopped by the executor: its `timeout`, the job's wall time, a cancellation, a shutdown | `error` |
| could not start: the program is not on the check's `PATH`, the sandbox refused it | `error` |
| no check matched the pair | `error` |

What the tree's code did is `pass` or `fail`; what happened to the run is `error` ([evidence.md](evidence.md): "the check reached no verdict").

Each requested pair gets exactly one summary object with its kind and target, which `DONE` requires. With one matched check its result is the summary's; with several, the summary is `fail` if any failed, else `error` if any errored, else `pass`, and each check also gets a finer object whose target is `<target>#<check>` (`#<check>` for the whole check).

Each check's log (stdout and stderr, interleaved, at most `[run] max_log`, 16 MiB; past that the rest is counted and dropped, and the log says how much) and its declared `artifacts` (regular files in the tree, opened component by component with no symbolic link anywhere on the way, copied out of the tree before they are hashed, at most `[run] max_artifact` each, 16 per check) are uploaded by digest before the object that names them. An upload that still fails after three tries is named anyway (the artifact then reads `absent`) and the note says so.

Every object's annotations, which Reef never reads, say what produced it:

```json
{
  "executor": { "name": "reef-executor@ci-7", "version": "0.4.2" },
  "job": "job_8f3a2c...", "attempt": 1,
  "sandbox": { "kind": "bubblewrap" },
  "verify": "disk",
  "repository_checks": "<SHA-256 of reef-checks.toml, when a check came from it>",
  "checks": [
    { "name": "crate", "source": "repository", "declaration": "<SHA-256>", "network": "none",
      "exit": 0, "wall_ms": 71204, "cpu_ms": 302511 }
  ],
  "note": "why a pair is error, or what was lowered"
}
```

A stopped check has `"stopped": "timeout"` (or `budget`, `cancelled`, `shutdown`) instead of `exit`; a signal, `"signal": 11`; one that never started, `"started": false`.

A job completes `DONE` once every pair has its summary, whatever the results. When its wall time runs out mid-check (the job's `budget.wall_time`, less a reserve of 30 seconds or a quarter of the time, whichever is less), the check is killed, its pair is `error` with `stopped: budget`, no further check starts, and the job completes `BUDGET_EXHAUSTED`. CPU time (`budget.cpu_time`) is checked between checks against what `wait4` reports; a check is not stopped mid-run for CPU. A job that can't be run at all completes `FAILED` with the step named: no commit, a listing or blob that did not match, a layout that failed, a disk check that failed.

A check never outlives its job. It runs in its own session; stopping it sends `SIGTERM` to its group, then `SIGKILL` after `[run] kill_grace` (10 seconds). Afterwards every process of the user whose working directory is inside the job's directory is killed (on Linux, bubblewrap's PID namespace has already taken them).

## Getting the tree

The executor reads the job's tree through `FileService` at the job's commit, the same way over the socket and the remote listener, and checks it three times before anything runs:

1. **The listing.** `ListFiles` recursively; the entries (path, type, git object id) are hashed into git tree objects, and the root must be the job's `tree_id`. A conflicted entry, a path with an empty, `.`, `..` or `.git` component, or a path that is both a file and a directory fails the job.
2. **Each blob.** Blobs not already cached are read with `ReadFile`, eight at a time, and each must hash as a git blob (collision-detecting SHA-1) to its listed id before it is written to the cache.
3. **The disk.** The job's directory is laid out from the cache (files created with `O_EXCL | O_NOFOLLOW` under directories the walk itself created, symbolic links created and never followed, submodules as empty directories) and hashed back into a tree id from the disk alone, which must be `tree_id` again. `[materialize] verify = "listing"` skips this pass for trees where it costs too much; the annotations record which ran.

A job submitted by tree alone (`reef job submit --tree`) has no commit to read and completes `FAILED`: submit by revision. The blob cache is content-addressed, shared by every job, bounded by `[materialize] cache_size` (4 GiB) and evicted least recently used after each job; it is never visible to a check. The first job on a tree reads every blob; later jobs read only what changed.

## The sandbox

`[sandbox] kind = "auto"` is Seatbelt on macOS and bubblewrap on Linux. At start the executor applies it once (`true` under a small policy); if that fails it refuses to start, exit 6, naming why and how to fix it. `kind = "none"` runs checks as plain child processes with the same environment rules, says so at every start, and records `sandbox: none` in every object and as the claim label `sandbox=none`. It is for machines that are already isolated: a fresh container or VM per run.

| | Inside the sandbox |
| --- | --- |
| the job's tree, home and temporary directory | read and write |
| a cached check's cache | read and write |
| the user's home (`HOME`, and the password database's when it differs) | hidden, except `[sandbox] read` (default `~/.rustup`, `~/.cargo`) |
| `[sandbox] deny` (default `~/.cargo/credentials` and `credentials.toml`, cargo's registry tokens) | hidden, even under a `read` path |
| Reef's state directory (the socket, the database, keys), its configuration, `REEF_SOCKET`'s path, the runtime directory (`$XDG_RUNTIME_DIR`), the executor's configuration and token file | hidden, even under a `read` path |
| the executor's work directory but this job's (other jobs, the blob cache, the logs) | hidden |
| the rest of the filesystem | read only |
| the network | `none` unless the check asks and the executor allows |
| other processes, the executor's included | not visible (Linux), not signalled or inspected (macOS) |
| setuid programs | not elevated |

Network levels: `none` on Linux is a network namespace with only its own loopback (a test can bind `127.0.0.1` and reach nothing outside); on macOS it is no network at all, Unix sockets included. `loopback` on Linux is the same as `none`; on macOS it is the host's loopback, which reaches whatever listens there. `full` is the host's network; on macOS it reaches no Unix socket but the resolver's (so not the user's ssh-agent or gpg-agent), and on Linux the host's abstract Unix sockets (some X11 and D-Bus servers) stay reachable.

Every claim carries the label `sandbox=<kind>`, so a requester can require one (`reef job submit --require sandbox=bubblewrap`), and policy can (R3.07).

### macOS: Seatbelt

`/usr/bin/sandbox-exec` runs each check with a profile generated for it. `sandbox-exec` is deprecated in its manual page but ships with every macOS release; Seatbelt is what Bazel's and Nix's macOS sandboxes use. The profile denies the network (all of it for `none`), all writes but the job's directories and a few devices, reads of the home and work directory but the readable and writable paths inside them, reads of the denied paths, signals and process inspection outside the sandbox, and the keychain's services.

Limits, which the probes show as `allowed (a documented limit)` where they apply:

- **Another process's arguments and environment stay readable** (`sysctl` `KERN_PROCARGS2`), the executor's and its ancestors' included: Seatbelt has no rule for it. So `reef executor run` runs itself again with only the variables it needs (`PATH`, `HOME`, the locale, `REEF_` settings but the token, `[sandbox] pass_env`), the token, if it came in `REEF_REMOTE_TOKEN`, on a pipe. That keeps secrets out of the executor's own environment, not out of its parent's: start it from a shell that holds none, with `--token-file`. Session keys and claim tokens live in the executor's memory, which `task_for_pid` does not reach for a process of the same user.
- Hidden paths are refused, not invisible: their names can still be statted where a path walk needs them.
- Mach services other than the keychain stay reachable.
- A process that leaves the job's directory (`cd /`) before the check ends escapes the sweep; a VM is the complete answer.

**Nesting.** macOS refuses to apply a sandbox inside another (`sandbox_apply: Operation not permitted`), so the executor cannot use Seatbelt when it already runs inside one: a CI runner's, or a coding agent's own. `check` reports that, and `kind = "none"` is the choice if the outer sandbox is the isolation you want.

### Linux: bubblewrap

`bwrap` 0.8 or later (`apt-get install bubblewrap`, `dnf install bubblewrap`), on `PATH` or at `[sandbox] bwrap`. `bwrap` itself starts with an empty environment, and the check's is set inside with `--clearenv` and `--setenv`, so no variable (`LD_PRELOAD` among them) acts on `bwrap` before the namespaces exist. Each check gets new user, PID, IPC, UTS and (but for `full`) network namespaces, a read-only view of `/`, a fresh `/dev`, `/proc`, `/tmp`, `/var/tmp` and `/run`, empty `tmpfs` mounts over the home, the work directory and the denied directories (so their contents are not even visible), read-only binds of `[sandbox] read`, and binds of the job's directories; all capabilities dropped and `no_new_privs` set. The check runs as the namespace's PID 1 (`--as-pid-1`), so `bwrap` reaps it and its CPU time reaches the executor; killing `bwrap` takes the whole namespace with it.

bubblewrap needs unprivileged user namespaces. Ubuntu 24.04 and later restrict them through AppArmor, and `bwrap` then fails with "setting up uid map: Permission denied". Either install an AppArmor profile for it (preferred on a long-lived machine), as `/etc/apparmor.d/bwrap`:

```
abi <abi/4.0>,
include <tunables/global>

profile bwrap /usr/bin/bwrap flags=(unconfined) {
  userns,
  include if exists <local/bwrap>
}
```

and `sudo apparmor_parser -r /etc/apparmor.d/bwrap`; or lift the restriction for the machine with `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` (fine on a throwaway CI VM). In Docker, the container must be allowed to create namespaces (`--privileged`, or a seccomp profile that allows `unshare` and `clone` with namespace flags).

Limits: the rest of the filesystem stays readable; a kernel or bubblewrap bug escapes; there is no seccomp filter, landlock ruleset, or disk and memory quota (cgroups) yet: a follow-up.

### The probes

`reef executor check` runs fourteen escape probes, each as `reef executor __probe` inside the configured sandbox, and again without it as a control. They aim at paths made for the run (a fake home, state directory, sockets and work directory) and at the real home and Reef paths as the executor hides them, so they never touch anything of value:

| Probe | Tries |
| --- | --- |
| `state-socket`, `runtime-socket` | connect to a Unix socket in the state directory, and at a runtime-directory socket |
| `host-loopback` | connect to a TCP listener on the host's loopback |
| `read-home`, `read-state`, `read-cache`, `read-other-job` | read a file there |
| `write-home`, `write-outside`, `write-work` | write a file there (checked on the host, so a private `tmpfs` does not count) |
| `executor-environment` | read the executor's arguments and environment |
| `signal-executor` | signal the executor |
| `setuid` | run a setuid program with its privileges |
| `environment` | see an executor variable (a token, a socket) in the check's own environment |

`check` exits 0 only when every probe the platform promises is refused (`holds`), 6 when the sandbox can't be applied or a probe escaped (the error names it and what it reached), and 2 for a configuration error. Each probe's detail says what the sandboxed and the control run saw.

## Keys

After each claim, before any work, the executor issues a session key with actor `executor`: on the socket for `[executor] identity` (empty: the default person identity), remotely for the token's linked identity. When less than ten minutes of the key remain and the job is still running, it issues the next key in the same session, so a long job is one session of several keys, each valid when the daemon witnesses what it signed. Keys and sessions are never shared between jobs.

A key lives only in the job's task and is dropped when the job ends, which wipes its PEM; ring's own key schedule is not wiped ([identity.md](identity.md)). Nothing writes it: not the work directory, not the logs, not a check's environment. The claim token is held the same way. The executor sets its core-file limit to zero and, on Linux, makes itself non-dumpable, so a crash leaves no core with a key in it and another process of the same user can't read its memory.

A key the executor can't get (no linked identity, a revoked identity, a refused actor) gives the job back and stops the executor with the daemon's error: it is a configuration problem every later job would hit. So does evidence the daemon refuses: that is an executor bug.

## The work directory

```
<work>/                     [work] dir; default ~/Library/Caches/reef/executor (macOS), $XDG_CACHE_HOME/reef/executor (Linux); mode 0700
  lock                      held for as long as the executor runs: two executors never share a work directory
  blobs/                    the verified blob cache
  caches/<repo>/<check>/    caches of cache = true checks
  jobs/<job>-<attempt>/     mode 0700, removed when the job ends (unless --keep-work)
    tree/                   the checked tree; the check's working directory
    home/  tmp/             the check's HOME and TMPDIR
    out/                    logs, never visible to a check
```

At start the executor removes job directories a crashed run left.

## `executor.toml`

Read from `--config`, else `REEF_EXECUTOR_CONFIG`, else `executor.toml` in the client's state directory (beside `config.toml`, a directory every sandbox hides), else the defaults. Parsed strictly: an unknown key is an error naming its line. `reef config show` prints the daemon's file, not this one; `reef executor check` prints this one as parsed.

```toml
[executor]
name = "laptop"                     # default reef-executor@<host>
labels = ["gpu=none"]               # added to os=, arch= and sandbox=
repos = []                          # only these repositories; empty for all
identity = ""                       # the session keys' identity; empty for the default
lease = "60s"                       # 15s to 10m

[repository]
checks = "allow"                    # allow | deny: read reef-checks.toml at the job's tree
kinds = ["build", "lint", "test"]   # kinds claimed for repository-declared checks

[declarations]
target_pattern = '^[A-Za-z0-9._/:@+=-]{1,512}$'

[materialize]
verify = "disk"                     # disk | listing
cache_size = "4GiB"

[work]
dir = ""                            # default above

[sandbox]
kind = "auto"                       # auto | seatbelt | bubblewrap | none
bwrap = ""                          # default: bwrap on PATH
read = ["~/.rustup", "~/.cargo"]    # readable inside the hidden home
pass_env = ["RUSTUP_HOME", "CARGO_HOME"]
env = { RUSTUP_HOME = "~/.rustup", CARGO_HOME = "~/.cargo" }   # set in every check; a pass_env variable the executor has wins
deny = ["~/.cargo/credentials", "~/.cargo/credentials.toml"]   # hidden even under a read path
max_repository_network = "none"     # none | loopback | full

[run]
max_log = "16MiB"
max_artifact = "64MiB"
kill_grace = "10s"

[[check]]                           # the executor's own checks, as above, plus cache = true
```

## Reef's own checks

Reef's repository has a [`reef-checks.toml`](../reef-checks.toml) (a rustfmt lint, and tests per crate). With the defaults, a sandboxed `cargo` reads its registry from `~/.cargo` but can't write to it, so these checks run offline against what the machine already fetched (`cargo fetch` first, outside the executor):

```sh
reef executor run --once &
reef job submit --kind lint --wait
reef job submit --kind test --target crates/reef-evidence --wait
```

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| `bwrap is not on PATH` | install bubblewrap, or set `[sandbox] bwrap` |
| `bubblewrap cannot create a user namespace` | the AppArmor profile or sysctl above; in Docker, `--privileged` |
| `Seatbelt cannot be applied here: sandbox_apply` | the executor is already inside a sandbox; run it outside, or `kind = "none"` if the outer one is enough |
| a check `did not start: cargo is not on the check's PATH` | the executor's `PATH` lacks it, or it lives in the hidden home: add its directory to `[sandbox] read` |
| a check fails writing to `~/.cargo` or another home path | the home is the job's own and empty; point the tool at a cache (`cache = true`) or run it offline |
| `another executor uses <work>` | each executor needs its own `[work] dir` |
| `this job names a tree with no commit` | submit by revision (`reef job submit -r`) |
| a probe `ESCAPED` | its detail says what it reached; on Linux, check the AppArmor and container notes above |
| evidence reads `UNBOUND` | the executor signed on the socket for an identity that no longer exists, or remotely with an unlinked token: link the token to a `ci` identity |
