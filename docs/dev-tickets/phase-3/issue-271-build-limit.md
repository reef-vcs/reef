# Issue #271: a machine-wide build limit (a daemon-owned jobserver, and job counts for the rest)

Phase 3 · Size M · Fixes: issue #271 (follow-on B of #264) · Depends on: #264 (ADR 0073, the fleet benchmark and its baseline), R1.21 (`reef ws run`), R2.26 (the provider contract), #260 (the `~/.reef` default root, ADR 0072), R2.11 (presence: the lease entries that count builders), R1.20 (store builds) · Builds on: ADR 0029 (v1 only grows), ADR 0045 (brief JSON), ADR 0050 (`reef schema`), ADR 0053 (the provider contract) · Interacts with: #272 (C, parallel, its own revision), #273 (D: clone-store builds draw from this pool) · Unblocks: G5 to G8 of the fleet gate (RX.06), R3.20 (compute budgets can read the limit)

## Context

Five agents building at once push a standard MacBook (8 cores, 16 GiB) into a load of 25 to 45 (#264's baseline: excalidraw p95 42.0, cli 24.7 on the standard VM; tokio p95 32.0 and the first memory stall, PSI `full` 29.6%, on the 4-core Linux runner). Every build assumes it owns the machine: cargo's `-j` defaults to the core count, rustc adds codegen threads, test harnesses run one thread per core. Oversubscription is not only slow: excalidraw's five tickets failed 35 tests by timeout that pass alone (G9).

ADR 0073 decided the mechanism (decisions 9 and 10): a jobserver the daemon owns, in GNU make's protocol over a named FIFO, handed to cargo as `CARGO_MAKEFLAGS`; job counts fixed at start for tools without a jobserver; `nice` 10 for `ws run` commands; the provider contract's `env` and a daemon-written `.cargo/config.toml` for commands Reef does not start. This brief turns that into a plan, and corrects it where an experiment on this machine contradicted it (listed under "Departures from ADR 0073" and recorded in ADR 0074).

The issue's acceptance: #264's G6 to G8 pass on the standard-machine profile with five Rust tickets, and a lone build is within 10% of unlimited. ADR 0073 and `dev-phases.md` say this ticket turns on G5 to G8; this brief gates G5 too (decision 14).

### What was verified while writing this ticket

Reef at `8fce9be1` (#264 merged): API revision 30 (`crates/reef-api/src/lib.rs:49`), schema 17, ADR 0073 the latest, threat rows to S-187. This brief takes **API revision 31**, **ADR 0074**, threat rows **S-188 to S-190**, and **no migration** (schema stays 17: every figure is runtime state). Paths are under `crates/` unless they start with `docs/`, `web/`, `sdk/` or `.github/`.

- **`reef ws run`** spawns its command in the CLI's process and process group (`reef-vcs/src/cli/ws_run.rs`, `spawn()`), with `reef_env` (`cli/env.rs`), the provenance variables and `--env`; `kill_on_drop(false)`. It reports an `ACTIVITY` presence entry while the command runs. No priority is set. `--env` is the only way to add variables.
- **The provider contract** (`reef-vcs/src/cli/provider.rs`, `Provided` at line 147) prints `contract: 1`, the path, lease and holder; no `env`. Keys a reader does not know are ignored, so adding `env` keeps contract 1. The shipped worktree adapter (its `hooks.json` under `plugins/`) reads `-o text` (the path alone), so no shipped adapter would consume `env` yet.
- **Builders are countable**: the daemon publishes a `Lease` presence entry on every heartbeat, which lapses after two missed heartbeats (`reef-daemon/src/presence/publish.rs`, `lease()`; `[lease] heartbeat` default 60 s). A `ws run`, a provider follower and `reef mcp` all heartbeat.
- **Configuration**: `config.toml`'s `KEYS` (`reef-daemon/src/config.rs:89`) has no `[build]`. `Config::workspace_base` is `Some(~/.reef)` only for the default root of the default state directory (#260); an explicit state directory (tests, the fleet harness) keeps workspaces at `<state>/repos/<id>/ws`. `reef.toml` is strict (`reef-lib/src/spec.rs`, `deny_unknown_fields`: `[workspace]`, `[snapshot]`, `[[store]]`), resolved per workspace at create (`reef-daemon/src/spec.rs`, `ResolvedSpec`).
- **Store builds** run with `env_clear()` plus `PATH`, `HOME`, `LANG`, `TMPDIR` and `[store.env]` (`reef-daemon/src/store/build.rs:532`), in their own process group, `kill_on_drop(true)`.
- **The proto**: `GetDaemonStatusResponse` ends at field 12 (`remote`); `WorkspaceService` has seven RPCs. `NOT_TOOLS` is at `reef-vcs/src/mcp/tools.rs:526`. Handlers tell a remote caller by `service::remote_of`.
- **Precedents**: `pre_exec` and `proc_pidinfo` in `reef-vcs/src/cli/holder.rs`; `setpriority` in `reef-daemon/src/symbols/mod.rs` (`lower_priority`).
- **The fleet harness** refuses `--limit on` (`reef-testing/src/fleet/run.rs:127`); its `DROPPED` lists do not drop `CARGO_MAKEFLAGS`, `MAKEFLAGS`, `RUST_TEST_THREADS` or `GOFLAGS`; G8's threshold text says its second half needs #271 (`report.rs`).
- **Toolchains here**: cargo and rustc 1.99.1, cargo-nextest 0.9.148, Go 1.27.4 (now installed), `/usr/bin/make` GNU Make 3.81, node 22.16.0, npm's bundled node-gyp 11.2.0, Vitest 5.0.5 (`web/`). In the cargo registry: `jobserver` 0.1.35, `cc` 1.2.64, `cmake` 0.1.58. No `cmake` binary; Docker was not running, so the Linux rows below are marked to verify.

#### The experiments

Spike crates under the session scratchpad (`spike-271/`), never in the workspace: a 16-crate workspace (`release`, `codegen-units = 16`, 2,500 generated functions per crate, about 9 s cold on 12 cores), `regex-automata` 0.4 alone (rustc's parallel codegen: 457% CPU unlimited), a `cc` build script compiling 16 C files with `parallel`, a build script that runs `make` with `MAKEFLAGS` taken from `CARGO_MAKEFLAGS` (what the `cmake` crate does), sleep-based libtest, nextest and Go tests. A token pool was a FIFO made and held open read-write by a small helper, which wrote N `+` bytes and counted what was in the pipe with `FIONREAD`. To reproduce without the helper:

```sh
F=$PWD/jobserver; mkfifo -m 600 "$F"; exec 3<>"$F"; printf '+++++++' >&3      # 7 tokens, held open by this shell
CARGO_MAKEFLAGS="-j --jobserver-auth=fifo:$F" cargo build --release           # count rustc: pgrep -x rustc | wc -l
python3 -c 'import os,fcntl,termios,struct,sys; fd=os.open(sys.argv[1],os.O_RDONLY|os.O_NONBLOCK); print(struct.unpack("i",fcntl.ioctl(fd,termios.FIONREAD,b"\0"*4))[0])' "$F"
```

| | Experiment | Result |
| --- | --- | --- |
| J1 | 16 crates, `CARGO_MAKEFLAGS=-j --jobserver-auth=fifo:<path>`, 7 tokens | **Honoured.** At most 8 rustc at once (7 tokens + cargo's implicit one; 12 unlimited); all 7 tokens back at the end |
| J2 | `regex-automata` alone, 0 tokens / 3 tokens / unlimited | 9.4 s at 99% CPU / 3.7 s at 261% / 2.5 s at 457%: **rustc's codegen threads draw from the pool** |
| J3 | 7 tokens and `CARGO_BUILD_JOBS=2` (env) | 8 rustc: **an inherited jobserver overrides `-j`**, silently from the environment or config |
| J4 | 7 tokens and `cargo build -j 1` | 8 rustc, and the warning "a `-j` argument was passed … ignoring the `-j` parameter" |
| J5 | 3 tokens and `CARGO_BUILD_JOBS=12` | 4 rustc, 16.4 s |
| J6 | Three cargos at once on 7 tokens | At most 10 rustc: **each client adds its own implicit token** |
| J7 | Path missing | cargo warns "failed to connect to jobserver … No such file or directory" on every invocation and runs unlimited (12) |
| J8 | A FIFO nobody holds or fills (an orphan) | **Silently serial**: one rustc at a time; the 16 crates had not finished after 60 s (9 s normally) |
| J9 | `SIGKILL` of cargo, `SIGTERM` or `SIGKILL` of its process group, mid-build, 7 tokens | **All 7 tokens lost** each time. Only the build's completion returns them |
| J10 | A fresh FIFO renamed over the path mid-build | The running cargo kept the old one (it opens the path once, at start) and returned its tokens there; the new pool was untouched |
| J11 | `lsof -p` of cargo and rustc during a build | Both hold the FIFO open (two descriptors each): holders are visible per process |
| J12 | `FIONREAD` on macOS | 0 on the `O_RDWR` descriptor that holds the FIFO, the real count on a separate `O_RDONLY` one (kqueue's `EVFILT_READ` agrees) |
| J13 | `cc` build script, no pool / 1 token | 12 compiles at once / 2: **`cc` follows the pool**. Cargo hands build scripts `CARGO_MAKEFLAGS="-j --jobserver-fds=fifo:<path> --jobserver-auth=fifo:<path>"` and `NUM_JOBS` = its `-j`, which `CARGO_BUILD_JOBS` sets even under a jobserver (3 with `CARGO_BUILD_JOBS=3`), with no warning |
| J14 | `/usr/bin/make` 3.81 with `MAKEFLAGS="-j --jobserver-auth=fifo:…"`, 8 one-second targets | **1.0 s instead of 8.1**: make 3.81 ignores the `fifo:` option and obeys the bare `-j`, which means unlimited jobs |
| J15 | make 3.81 with what cargo hands a build script (J13's string) | **`make: *** internal error: invalid --jobserver-fds string 'fifo:…'. Stop.`** A build script that passes `CARGO_MAKEFLAGS` to make fails |
| J16 | The `cmake` crate 0.1.58's source | Passes `CARGO_MAKEFLAGS` to make as `MAKEFLAGS` for a Makefile project; on macOS **only when the jobserver is a named pipe** ("unless a named pipe jobserver is available"); on Linux always. So every `-sys` crate built through `cmake` with Makefiles fails on macOS under a `fifo:` jobserver with make 3.81 (J15's error; `cmake` itself not installed here, so reproduced with an equivalent build script) |
| J17 | The fd form instead (`--jobserver-auth=7,8`, descriptors opened by the parent) | Works for cargo (4 rustc on 3 tokens) and for make 3.81 (J15's build script passes). But **through a Python `subprocess` or a Node `child_process` it fails**: they close inherited descriptors, and cargo warns "cannot open file descriptor 7" and runs unlimited. Through Node, the `fifo:` form works |
| J18 | Cargo `[env] CARGO_MAKEFLAGS = …` (with and without `force`) in a parent `.cargo/config.toml` | **No effect** on cargo or rustc (12 rustc): `[env]` cannot carry the jobserver |
| J19 | A parent directory's `.cargo/config.toml` `[build] jobs = 2` | Honoured from a workspace below it (2 rustc). A repository's own `jobs = 5` overrides it (5). A `CARGO_HOME` config's `jobs = 6` **does not** (2): a deeper directory wins over `$CARGO_HOME`, and `~/.reef` is deeper than `~/.cargo` |
| J20 | libtest: `RUST_TEST_THREADS=2`; the same in a parent config's `[env]`; `[env]` with the variable already set to 8 | 8.1 s (16 one-second tests; 2.0 s default) / 8.2 s / 2.1 s: `[env]` reaches test binaries and never overrides the environment |
| J21 | nextest: `NEXTEST_TEST_THREADS=2`; `RUST_TEST_THREADS=2`; `[env] NEXTEST_TEST_THREADS` | 8.2 s / 2.0 s / 2.0 s: nextest reads only its own variable, from the environment. Its build step follows the pool (2 rustc on 1 token) |
| J22 | Go: `GOFLAGS=-p=1`; `-p 6` on the command line; with `go vet`, `go mod tidy`, `go run` | 6.6 s for six packages with a one-second test (about 1 s at `-p 6`, which overrides `GOFLAGS`); the other commands accept it. **No jobserver**: `go` reads no `MAKEFLAGS` |
| J23 | `go env -w GOFLAGS=-trimpath`, then `GOFLAGS=-p=2` in the environment | `go env GOFLAGS` is `-p=2`: **the variable replaces the user's setting**, dropping `-trimpath`, which changes what is built |
| J24 | node-gyp 11.2.0, Vitest 5.0.5 | node-gyp reads `JOBS` (unset, it runs make serially: setting it would add parallelism). Vitest reads `VITEST_MAX_WORKERS` and lets it override the configuration's `maxWorkers`. Neither has a jobserver |
| J25 | `CARGO_MAKEFLAGS=` (empty) with `cargo build -j 2` | 2 rustc, no warning: an empty variable is the per-command way out |
| J26 | Lone build, 3 runs: unlimited vs. the pool at 11 tokens (`jobs` 12) with `nice -n 10` | 9.18 / 9.22 / 9.24 s vs 9.29 / 9.15 / 9.21 s: no difference |
| J27 | Five 16-crate builds at once (dev host, 12 cores, 32 GiB; `dev`, decides nothing) | Unlimited: at most 60 rustc, load peak 34.8, makespan 36.5 s. Pool of 11 (`jobs` 12): 16 rustc, 37.8 s (1.04×). Pool of 7 (`jobs` 8): 12 rustc, load peak 9.5, 38.2 s (1.05×). Every token came back. The load figures of the second and third runs started from the first's decay, so read the rustc counts |

## Decisions

### Size (decision 1)

**M**: one daemon module (the FIFO, recovery, the environment, the cargo file), one RPC and a status field, `ws run`'s use of them, the provider's `env`, `reef ws env`, configuration in both files, the harness's `--limit` and the measurement. The executor's sandbox, cgroups and `reef here` changes are out (ADR 0073). Store builds draw from the pool here, since it is three lines in `store/build.rs` and D's clone-store builds depend on it.

### The jobserver (decisions 2, 4, 5)

**One FIFO at a stable path**, `<state>/build/jobserver` (the directory `0700`, the FIFO `0600`, both checked at start as S-01 checks the state directory: owner the daemon's uid, type FIFO; anything else is moved aside to `*.trash-<nanos>` and recreated). Stable, because commands open the path when they start (J10): a `ws run` command that lives for hours (often the agent itself) starts cargo many times, and each must find the path.

**The protocol** is GNU make's POSIX jobserver: a token is one byte, `+`; a client reads a byte to start a job and writes the same byte back when the job ends; every client has one implicit token it never reads (J6). Reef never reads tokens for itself except to drain a surplus.

**The pool holds `jobs − 1` tokens** (decision 4), so a lone build runs exactly `jobs` jobs (J1, J26) and five builds at most `jobs + 4` (J6, J27: 12 on the standard machine). With `jobs = 1` the pool is empty and every build is serial.

**The daemon holds the FIFO open** for its whole life, read-write (`O_RDWR | O_NONBLOCK`, close-on-exec): a FIFO's buffer exists only while a descriptor is open, so without this the tokens vanish whenever no build runs. It counts with `FIONREAD` on a second, read-only descriptor (J12).

**Recovery** (decision 5). Every token a killed build held is lost (J9), and agents kill builds often (timeouts, interrupts), so recovery is part of the design, not a corner:

- **Top up when nobody holds it.** Every 5 s the daemon reads the count. Below the pool for two checks in a row, it lists the processes holding the FIFO (J11: every descriptor on its device and inode; macOS `proc_listallpids` and `proc_pidinfo(PROC_PIDLISTFDS)` with `proc_pidfdinfo` for vnode descriptors, Linux `/proc/<pid>/fd/*`; same-uid processes only, the daemon excluded). None: every missing token is lost, and it writes them back. This is exact, and it fires whenever the machine is between builds, which agent fleets are most of the time.
- **Drain any surplus.** More than the pool (a client returned a token after a top-up, or a buggy or hostile one wrote extra): the daemon reads the excess and drops it. A surplus is therefore temporary, so topping up at the wrong moment costs a short oversubscription, never a permanent one.
- **At every start**, the daemon reuses the FIFO at the path if it passes the checks, tops it up to the pool, and drains as above: a build that opened the FIFO while the daemon was down (and is starving on it, J8) is rescued. A FIFO that fails the checks is replaced by `mkfifo` at `jobserver.<nanos>.tmp`, a fill, and a rename over the path.
- **At a graceful stop**, the daemon unlinks the path before closing its descriptors. Builds already running keep their own descriptors and pool; a cargo started while the daemon is down gets J7's warning and runs unlimited, which is better than J8's silent serial build. A crash leaves the FIFO in place, and the next start rescues it.
- **`generation`** counts replacements of the FIFO (1 at first start), and `top_ups` and `tokens_restored`, `tokens_drained` count the rest; all in `reef daemon status`.

ADR 0073's "a new FIFO (`jobserver-<generation>`) when no `ws run` command and no holder has been live for a minute" is replaced: when the agent itself is the `ws run` command it is live for hours, so the timer would never fire; and a new path breaks every long-lived environment that names the old one.

### Handing it to commands (decisions 2 and 3)

**`CARGO_MAKEFLAGS=-j --jobserver-auth=fifo:<path>`**, the `fifo:` form, set by `ws run` (decision 2). The fd form (`--jobserver-auth=R,W`) is what make 3.81 understands, but it dies at the first Python or Node process between `ws run` and cargo (J17), which is where an agent's tools run. Only the path survives any process tree.

**Not `MAKEFLAGS`** by default: make 3.81, macOS's `/usr/bin/make`, ignores `fifo:` and treats the bare `-j` as unlimited (J14), so a repository's plain `make` would get faster at oversubscribing. `[build] make_jobserver = true` in `config.toml` sets `MAKEFLAGS` to the same value for a machine whose `make` is GNU make 4.4 or later (the daemon checks `make --version` and refuses the setting at start otherwise, with a warning).

**The make hazard** (decision 3). Cargo hands build scripts `--jobserver-fds=fifo:<path>` (J13), which GNU make before 4.4 rejects (J15), and the `cmake` crate passes it to make exactly when the jobserver is a FIFO on macOS, and always on Linux (J16). Ubuntu 24.04 ships make 4.3, to verify in the container. So under a `fifo:` jobserver a repository that builds a `-sys` crate through `cmake` with Makefiles fails to build on macOS with the system make. Reef does not break builds to bound them:

- **Detection**: for a workspace whose root `Cargo.lock` lists a package known to hand `CARGO_MAKEFLAGS` to make (`cmake`, verified at 0.1.58; the implementer reads `autotools`, `openssl-src`, `tikv-jemalloc-sys`, `jemalloc-sys` and `protobuf-src` at their current versions and adds each that does, with the version read in a comment), and no `gmake` or `make` on the daemon's `PATH` that is GNU make 4.4 or later (checked once per 10 minutes), the daemon offers no jobserver for that workspace: `jobserver_skip` `MAKE_TOO_OLD`, the counts still set (with `CARGO_BUILD_JOBS` = share, which bounds cargo itself), and a note naming the package and the make found, with the fix (install GNU make 4.4 or later as `gmake`).
- **Declaration**: `[build] jobserver = false` in `reef.toml` turns it off for a repository whose build does this some other way (a `Cargo.lock` below the root, a script of its own).
- **Upstream**, two issues filed by the implementer: the `jobserver` crate should not emit `--jobserver-fds=` for a `fifo:` client (GNU make never accepted that spelling), and the `cmake` crate should check make's version before passing a named-pipe jobserver on macOS.

### Job counts for tools without a jobserver (decisions 6 and 7)

**`share = max(2, jobs / builders)`** (integer division), computed when the command starts and fixed for its life; the jobserver is the part that adapts. **`builders`** is the number of workspaces with a live `Lease` presence entry (a `ws run`, a provider follower or `reef mcp` heartbeating), machine-wide across repositories, counting the asking workspace once even before its first heartbeat, at least 1 (decision 11). On the standard machine: 8 alone, 4 with two, 2 with five.

The variables (decision 7), each set only when the command's environment lacks it:

| Variable | Value | Not set when |
| --- | --- | --- |
| `CARGO_MAKEFLAGS` | `-j --jobserver-auth=fifo:<path>` | no jobserver (the `jobserver_skip` reasons); the environment's `MAKEFLAGS` or `MFLAGS` already carries a `--jobserver-auth` (the command runs under a make); `build.jobs` is set in the workspace's `.cargo/config.toml` or `.cargo/config` or in `$CARGO_HOME/config.toml`, since a jobserver would silently lift that cap (J3); reason `CARGO_JOBS_SET` |
| `CARGO_BUILD_JOBS` | share | `build.jobs` is set as above. Under the jobserver it does not limit cargo (J3) but bounds `NUM_JOBS`, which build scripts that run their own `make -j` or `cmake --parallel` read (J13); without one it is cargo's limit |
| `RUST_TEST_THREADS` | share | the workspace's `.cargo/config.toml` `[env]` or `$CARGO_HOME/config.toml`'s sets it (a repository that needs serial tests says `RUST_TEST_THREADS = "1"` there, and the environment would override it, J20); or the workspace is under the cargo file below and that file carries the key (decision 6, as amended by the user: the file is rewritten as builders change and cargo reads it at every invocation, so a long-lived session's test threads follow the current share instead of the share at its start) |
| `NEXTEST_TEST_THREADS` | share | `.config/nextest.toml` sets `test-threads` in any profile |
| `GOFLAGS` | the user's `GOFLAGS` from the go environment file (`$GOENV`, else `<user config dir>/go/env`; none when `GOENV=off`) plus `-p=<share>`, unless it already has a `-p` | the file cannot be read or parsed. In the command's environment an existing `GOFLAGS` gets ` -p=<share>` appended unless it has a `-p` (merge `GOFLAGS`), because replacing it drops settings such as `-trimpath` (J23) |
| `MAKEFLAGS` | as `CARGO_MAKEFLAGS` | unless `[build] make_jobserver = true` |
| each `[build] jobs_env` name | share or jobs | already set |

Not set by default (decision 7): `VITEST_MAX_WORKERS` (it overrides a repository's own `maxWorkers`, J24), `JOBS` (node-gyp is serial without it, so setting it adds parallelism, J24), `GOMAXPROCS`, `CMAKE_BUILD_PARALLEL_LEVEL`, `UV_CONCURRENT_BUILDS`. A repository that wants them declares them:

```toml
[build]
jobs_env = { VITEST_MAX_WORKERS = "{share}", CMAKE_BUILD_PARALLEL_LEVEL = "{share}" }
jobserver = true          # default; false where a build hands the jobserver to an old make
```

`jobs_env`: at most 16 names, each matching `^[A-Z][A-Z0-9_]{0,63}$`, not one Reef sets itself (the table above), not `PATH`, `HOME`, `SHELL`, `USER`, `LOGNAME`, `PWD`, `TMPDIR`, `IFS`, `ENV`, `BASH_ENV`, not ending in `_OPTIONS` or `FLAGS`, not starting with `REEF_`, `LD_`, `DYLD_`, `GIT_`, `SSH_`, `CARGO_` or `RUSTC`; each value exactly `"{share}"` or `"{jobs}"` (S-188). Read from the workspace's base commit like the rest of `reef.toml`. An older daemon refuses a `reef.toml` with `[build]`, its parse error naming the key (ADR 0073 decision 12).

**Reef recommends but never sets** what changes a build's output (ADR 0073): `CARGO_INCREMENTAL`, profiles, `RUSTFLAGS`, `-trimpath`. Everything above changes only how many things run at once.

### Priority (decision 16)

Commands `ws run` starts, and store builds, run at **`nice` 10** (`[build] nice`, 0 to 19, 0 for none): in the child before `exec`, `setpriority(PRIO_PROCESS, 0, max(current, nice))`, so it never raises a priority. A person's terminal and editor stay responsive while agents build; a lone build loses nothing (J26). Background QoS is not used (ADR 0073).

### `jobs` (decision 7)

**`jobs = "auto"`**: the smaller of the logical cores (`available_parallelism`, which honours a cgroup's CPU quota) and the memory in GiB ÷ 2, at least 1; memory is `hw.memsize` on macOS, and on Linux the smaller of physical memory and the cgroup's `memory.max`. 8 on the standard machine, 12 on the dev machine, 4 on the Linux runner. `jobs = <n>` (1 to 1,024) fixes it. A change takes effect at the next daemon start.

### Reaching commands Reef does not start (decisions 8, 9, 10)

**The provider contract's `env`** (decision 9): `create` and `attach` print `"env": {"NAME": "value", …}`, the same variables `ws run` would set at that moment, assembled against the provider command's own environment (a hook inherits its program's). Contract stays 1. `docs/workspace-provider.md` says: an adapter whose program can set a session's environment applies each variable the session lacks; the counts are fixed at session start and the jobserver adapts. No shipped adapter changes in this ticket (the worktree adapter reads the path only).

**A cargo configuration above the workspaces** (decision 8): **`~/.reef/.cargo/config.toml`**, one file above every repository's directory, written only when `workspace_base` is set (the default root Reef owns, S-185) and `[build] limit` and `[build] cargo_config` are true:

```toml
# Written by Reef (#271): the build limit for cargo commands Reef did not start.
# Rewritten as workspaces come and go. A repository's own .cargo/config.toml overrides it.
[build]
jobs = 2

[env]
RUST_TEST_THREADS = "2"
```

- `jobs` is the share and `RUST_TEST_THREADS` too, rewritten when it changes (debounced 5 s), atomically (temporary file, rename), mode `0600`. Each key is left out when `$CARGO_HOME/config.toml` sets it (J19: Reef's file would otherwise override the user's own). A file without Reef's first line is never overwritten; the daemon logs a warning and `reef daemon status` says why there is none.
- What cargo makes of it: cargo reads it from any directory below (J19), at every invocation, a repository's own file wins, and the environment wins over both. Under `ws run` the jobserver governs and `build.jobs` is ignored (J3). `RUST_TEST_THREADS` comes from this file, not the environment, wherever the file carries it (the table above), so it adapts as builders come and go; nextest reads only the environment (J21), so `NEXTEST_TEST_THREADS` stays fixed at the session's start, a documented limit. For a session that `ws run` did not start (the provider, `reef mcp`), it is the whole limit: fixed shares, no jobserver (J18: `[env]` cannot carry one).
- Removed at start when the limit or the file is turned off (only if it carries Reef's line). Left in place at a stop, rewritten at the next start. A configured root, or an explicit state directory, gets none.

**`reef mcp`** cannot set its client's shell environment: under the default root it relies on the cargo file. `GetBuildEnvironment` is in `NOT_TOOLS` ("a tool result cannot set a shell's environment").

**`reef ws env [WS]`** (decision 10, `help-all` only): the variables for a workspace (default: the one the current directory is in) as `export NAME='value'` lines in text, an object in JSON, for `eval "$(reef ws env)"` in a shell Reef did not start.

### Degrading and opting out (decision 9)

- **Never fail a command for the limit.** `ws run` that cannot get the environment (the RPC fails, or the daemon is older than revision 31 and answers `UNIMPLEMENTED`) prints one `reef:` note on stderr and runs the command with `nice` only: counts from the local core count would equal each tool's default, so they are not set.
- **A remote daemon**: the jobserver is on another machine and its builders are not this machine's. Through the remote listener `GetBuildEnvironment` answers `jobserver_skip` `REMOTE` and no variables.
- **The jobserver missing or stale**: the daemon checks the FIFO before answering; one it cannot keep (`mkfifo` fails, a full disk) gives `jobserver_skip` `UNAVAILABLE` and counts only, with the error in the note. A path that disappears under a running command gives cargo's J7 warning and an unlimited build; the next answer has a new FIFO.
- **Opt-outs**: `reef ws run --no-build-limit` (no variables, no `nice`); `[build] limit = false` in `config.toml` (no FIFO, no cargo file, `enabled` false); `[build] jobserver = false` in `reef.toml`; `CARGO_MAKEFLAGS=` (empty) before one cargo command (J25), which `docs/build-limit.md` names for an agent that needs `-j 1`.

### The API: revision 31 (decision 10)

`workspaces.proto`:

```proto
// In WorkspaceService:
  // The environment that puts a command in `workspace` under the machine's
  // build limit (#271, ADR 0074; docs/build-limit.md): the daemon's
  // jobserver for cargo, job counts for tools without one, and the priority.
  // A client sets each variable its command's environment lacks, merging as
  // `merge` says; `reef ws run`, `reef ws env` and the provider contract's
  // `env` do. With `[build] limit = false`, `enabled` is false and
  // `variables` empty. Through the remote listener there is no jobserver
  // and no variable (`jobserver_skip` REMOTE). READ: answering changes
  // nothing; the workspace's files are read, never written.
  //
  // Errors: the workspace reasons.
  rpc GetBuildEnvironment(GetBuildEnvironmentRequest) returns (GetBuildEnvironmentResponse) {
    option (reef.api.v1.access) = ACCESS_READ;
    option idempotency_level = NO_SIDE_EFFECTS;
  }

// Ask for a command's build environment.
message GetBuildEnvironmentRequest {
  // The repository; may be empty.
  string repo = 1;
  // The workspace the command runs in. Empty: the machine's figures and
  // the default variables, without the workspace's own checks.
  string workspace = 2;
}

// What a command's environment should hold.
message GetBuildEnvironmentResponse {
  // `[build] limit`.
  bool enabled = 1;
  // In the order to apply them.
  repeated BuildVariable variables = 2;
  // The machine's job count (`[build] jobs`).
  uint32 jobs = 3;
  // The count each tool without a jobserver gets: max(2, jobs / builders).
  uint32 share = 4;
  // Workspaces whose leases are heartbeating, this one included.
  uint32 builders = 5;
  // The niceness to run the command at; 0 for none.
  uint32 nice = 6;
  // The jobserver's absolute path; empty when there is none.
  string jobserver = 7;
  // Why there is none; UNSPECIFIED when there is one.
  JobserverSkip jobserver_skip = 8;
  // One sentence for a person: which package and make, which setting.
  string note = 9;
}

// One variable of a build environment.
message BuildVariable {
  string name = 1;
  string value = 2;
  BuildVariableMerge merge = 3;
}

// How a client applies a variable its environment already has.
enum BuildVariableMerge {
  // Not set.
  BUILD_VARIABLE_MERGE_UNSPECIFIED = 0;
  // Keep the environment's value.
  BUILD_VARIABLE_MERGE_IF_UNSET = 1;
  // `GOFLAGS`: append ` -p=<share>` to the environment's value unless it
  // has a `-p` flag.
  BUILD_VARIABLE_MERGE_GOFLAGS = 2;
}

// Why a build environment has no jobserver.
enum JobserverSkip {
  JOBSERVER_SKIP_UNSPECIFIED = 0;
  // `[build] limit = false`.
  JOBSERVER_SKIP_DISABLED = 1;
  // Asked through the remote listener.
  JOBSERVER_SKIP_REMOTE = 2;
  // The repository's `[build] jobserver = false`.
  JOBSERVER_SKIP_REPOSITORY = 3;
  // A dependency hands the jobserver to a make older than 4.4.
  JOBSERVER_SKIP_MAKE_TOO_OLD = 4;
  // `build.jobs` is set in the workspace's or the user's cargo configuration.
  JOBSERVER_SKIP_CARGO_JOBS_SET = 5;
  // The daemon could not keep the FIFO; `note` says why.
  JOBSERVER_SKIP_UNAVAILABLE = 6;
}
```

`daemon.proto`: `GetDaemonStatusResponse.build` (field 13), a `BuildStatus`: `enabled`, `jobs`, `tokens` (the pool), `available` (in the FIFO now), `builders`, `share`, `jobserver` (path), `generation`, `top_ups`, `tokens_restored`, `tokens_drained`, `cargo_config` (path, empty when none), `note`. `API_REVISION` 31 with its digest appended; `just api` and `just api-breaking` against the root's `.git`.

The remote listener: READ RPCs need no `remote/rules.rs` row; the handler checks `service::remote_of`. The web listener serves it (READ); the web UI does not call it.

### The CLI (decisions 10, 12)

- **`reef ws run`**: calls `GetBuildEnvironment` for its workspace after create or attach and before the spawn (one RPC, under 10 ms), assembles the variables (`reef_client::build::assemble`, shared with the provider and `reef ws env`), then `reef_env`, the provenance variables and `--env`, so `--env` always wins; applies `nice` in `pre_exec`. `--no-build-limit`: "Run without the machine-wide build limit: no jobserver, no job counts, normal priority." Its `-o json` report is unchanged (the token benchmark measures it), and it prints nothing about the limit unless the RPC fails.
- **`reef daemon status`**: a `build` line in text (`build: 8 jobs, 5 of 7 tokens free, 3 builders (share 2); jobserver <path>; cargo config ~/.reef/.cargo/config.toml`) and the `build` object in JSON and brief.
- **`reef ws provider create|attach`**: the `env` object.
- **`reef ws env`**: above.
- **`reef here`, the guide, the root help**: unchanged (decision 12). Agents learn nothing new; the limit works without them.
- **`reef schema`**: rows for `daemon status` (`build`), the provider's `env`, `ws env`; `ws run`'s argument list gains the flag.

### Configuration

`config.toml`:

```toml
[build]
limit = true            # the jobserver, the counts, nice and the cargo file
jobs = "auto"           # or 1..=1024
nice = 10               # 0..=19; 0 for none
make_jobserver = false  # also set MAKEFLAGS (GNU make 4.4 or later only)
cargo_config = true     # ~/.reef/.cargo/config.toml, under the default root only
```

`reef config show` prints them with their sources; `KEYS` and its test gain the section. `reef.toml` gains `[build]` with `jobs_env` and `jobserver` (above).

### The ADR (decision 13)

**ADR 0074, "The build limit: one FIFO, recovered by its holders"**, amending ADR 0073's decisions 9 and 10 with what the experiments showed: the stable path reused across restarts and unlinked at a stop; recovery by an exact holder scan, a top-up at start and a surplus drain instead of generations after a minute idle; the pool at `jobs − 1`; the make hazard and its guard; settings already made in the workspace's or the user's configuration respected, `GOFLAGS` merged; `CARGO_BUILD_JOBS` at the share for `NUM_JOBS`; the cargo file at `~/.reef/.cargo/config.toml` with `RUST_TEST_THREADS` in `[env]`; the fd form rejected. ADR 0073 gains "Amended by 0074" in its header; the index gains the row.

### Threat rows (S-188 to S-190)

| Id | Area | Actor | Severity | Finding | Status |
| --- | --- | --- | --- | --- | --- |
| S-188 | Repository-declared build variables | Repository content (a commit an agent wrote) | Low | `[build] jobs_env` puts a variable in every command's environment in that repository's workspaces, which could change what a tool does or break it. | Fixed: names restricted (pattern, at most 16, none Reef sets, a denylist of shell, loader, git, SSH, cargo and `*_OPTIONS`/`*FLAGS` names), values only `{share}` or `{jobs}` (a number), never overriding a set variable, read from the base commit like the rest of `reef.toml`. Tests: `spec::tests::jobs_env_*`, `build_limit::jobs_env` |
| S-189 | The jobserver FIFO | A same-uid process | Low | A process can drain every token (each build then runs one job), write extra ones, or replace the FIFO; a killed build loses its tokens (J9). | Bounded: one uid is one trust domain (ADR 0027); the FIFO is `0600` in a `0700` directory, its owner and type checked at each start; the daemon tops up when no process holds it and at start, drains any surplus, and reports `available`, `top_ups` and drains in `reef daemon status`; `--no-build-limit`, `CARGO_MAKEFLAGS=` and `[build] limit = false` opt out. Tests: `build::jobserver::*`, `build_limit::recovers_after_kill` |
| S-190 | A cargo configuration outside the state directory | Reef itself, the user's setup | Low | `~/.reef/.cargo/config.toml` applies to every cargo command run under `~/.reef`, including a person's, and outranks their `~/.cargo/config.toml`. | Fixed: written only under the default root Reef owns (S-185's checks), with two keys, each omitted when the user's `$CARGO_HOME/config.toml` sets it, never over a file without Reef's header, removed when turned off; a repository's own file wins. Documented in `docs/build-limit.md`. Tests: `build::cargo_config::*` |

The review paragraph: "#271's build limit (rows S-188 to S-190) has had no external review. RX.02 reviews the declared variables' restrictions and the FIFO's checks."

### Measurement (decision 14)

**The harness** (`reef-testing/src/fleet/`):

- `--limit on|off|both`, default `on`. `off` passes `--no-build-limit` to every `ws run` (the default is on after this ticket, so the baseline stays reproducible). `both` runs the solo phase and the five-at-once phase twice, off then on, in one run against one daemon, so G8's makespan ratio and the lone-build ratio come from one report. `--solo-runs <n>` (default 1, 3 for gated runs) repeats each solo ticket and takes the median.
- `DROPPED` gains `CARGO_MAKEFLAGS`, `MAKEFLAGS`, `MFLAGS`, `RUST_TEST_THREADS`, `GOFLAGS` and `VITEST_MAX_WORKERS`, so a harness started inside a `ws run` measures its own daemon's limit (a repository's `env` in `repos.toml` is applied after the drop, as today).
- The sampler reads `reef daemon status -o json`'s `build` every 5 s with the canary: tokens free and builders over time, into `report.json` and `summary.md`'s pressure section.
- G8's threshold reads "median ticket ≤ 5× solo; with `--limit both`, five-at-once makespan on ≤ 1.1× off, and the solo ticket on ≤ 1.1× off (median of `--solo-runs`)"; each half is `n/a` without `both`.

**The gate run**, on the standard profile (the Lume VM, 8 vCPUs, 16 GiB, 512 GB sparse disk; runbook in `docs/fleet-benchmark.md`), tokio first: `reef-fleet run --repo tokio --profile standard --limit both --solo-runs 3 --out ~/fleet/271-tokio`. Before it, the user's rule: clean caches, then `df -h /System/Volumes/Data` at least 150 GiB free on the host (149 GiB while this was written, so a cleanup comes first). Then the other stacks with `--limit both` (report only), and `gh workflow run fleet.yml` with `--limit both` for the `linux` profile (4 vCPUs: `jobs` 4, pool 3). Pass: G5 to G8 for tokio on both profiles, G9 unchanged.

**Expected**, from J27 and the baseline: on the standard VM a lone tokio build gets 8 jobs (equal to unlimited, J26); five get at most 12 compile processes instead of about 40, and two test threads each, so the load p95 should fall from tokio's Linux baseline of 8× the cores (p95 32.0 on 4) to under 2×, the memory stall go, and the makespan stay within 1.1× (J27: 1.05× on the dev host).

**Cheaper checks first**: the integration tests (below), which count concurrent rustc through a wrapper; `reef-fleet run --repo synthetic --limit both` (the smoke, about 40 s); `--repo tokio --profile dev --limit both` on the host (labelled `dev`, decides nothing).

## Deliverables

### `crates/reef-api/`

- `proto/reef/api/v1/workspaces.proto`: `GetBuildEnvironment`, its messages and enums. `proto/reef/api/v1/daemon.proto`: `BuildStatus`, `GetDaemonStatusResponse.build = 13`. `src/lib.rs`: `API_REVISION` 31, the digest.

### `crates/reef-lib/`

- `src/spec.rs`: `BuildSpec { jobs_env: BTreeMap<String, String>, jobserver: bool }` under `Spec.build`, validated (S-188); the module doc's example. Its errors name the key.

### `crates/reef-daemon/`

- `src/build/` (new): `mod.rs` (`BuildLimit`: config, `jobs`, the share, builders from the presence hub, the 5 s check task), `jobserver.rs` (the FIFO: checks, create or reuse, hold, count, top up, drain, unlink at stop), `holders.rs` (the holder scan: macOS `libc` `proc_*`, Linux `/proc`, both `#[cfg]`, one `fn holders(dev, ino) -> io::Result<Vec<u32>>`), `env.rs` (the variables for a workspace: the workspace's cargo, nextest and `Cargo.lock` checks, cached by mtime and size; `$CARGO_HOME`'s config; the go environment file; `jobs_env`), `make.rs` (the GNU make version probe, cached 10 min), `cargo_config.rs` (the file under `~/.reef`).
- `src/config.rs`: `[build]` (`limit`, `jobs`, `nice`, `make_jobserver`, `cargo_config`), `KEYS`, defaults, `config show`.
- `src/spec.rs`: `ResolvedSpec.build`.
- `src/daemon.rs`: `AppState.build`; start (after the state directory's checks, before serving) and stop (unlink before the last descriptor closes).
- `src/service/workspaces.rs`: the handler. `src/service/daemon.rs`: `build` in the status.
- `src/store/build.rs`: the store build's environment gains the variables (`[store.env]` still wins) and `nice`.

### `crates/reef-client/`

- `get_build_environment`; `build.rs` (new): `assemble(response, lookup) -> Vec<(String, String)>` (if-unset and the `GOFLAGS` merge); `json` types for `BuildStatus`.

### `crates/reef-vcs/`

- `src/cli/ws_run.rs`: the call, `assemble`, `pre_exec` niceness, `--no-build-limit`, the stderr note on failure.
- `src/cli/provider.rs`: `Provided.env`. `src/cli/ws.rs`: `ws env`. `src/cli/daemon.rs`: the `build` line and object.
- `src/mcp/tools.rs`: the `NOT_TOOLS` row. `src/cli/schema/outputs.rs`: the rows; `help.txt` if `help-all` lists commands there.

### `crates/reef-testing/`

- `src/fleet/run.rs` (`--limit` modes, `--solo-runs`, `DROPPED`), `src/fleet/sample.rs` (the `build` sampler), `src/fleet/report.rs` (G8's halves, the lone ratio), `src/bin/fleet.rs` (the flags), `README.md`.

### Documentation

- **`docs/build-limit.md`** (new): what the limit does and why; the variables and when each is not set; the jobserver, the pool and recovery; the cargo file; the provider's `env` and `reef ws env`; the make hazard and its fix; the opt-outs (`--no-build-limit`, `CARGO_MAKEFLAGS=`, `[build] limit = false`, `[build] jobserver = false`); what a repository can declare; agent sandboxes (a sandbox that denies writes outside the workspace denies opening the FIFO read-write, and cargo then warns and runs unlimited: allow `<state>/build/jobserver`).
- `docs/quickstart.md`: a short "Builds share the machine" section and the `[build]` keys under Configuration. `docs/workspace-provider.md`: `env`. `docs/fleet-benchmark.md`: `--limit both`, `--solo-runs`, G8's halves, the #271 results tables (standard and Linux). `docs/threat-model.md`: S-188 to S-190 and the review paragraph.
- `docs/adr/0074-build-limit-jobserver.md` (accepted), indexed; ADR 0073's header names it.
- `CHANGELOG.md` Unreleased: Added (the build limit, `reef ws env`, `--no-build-limit`, `[build]`), Changed (`ws run` commands run at `nice` 10). `README.md`'s Phase 3 section: one line (#230's rule).
- `docs/dev-tickets/phase-3/README.md`: this brief's row (added with the brief).

## Tests

Sizes: S under a second, M a few seconds, L builds real crates (tens of seconds, offline). New fixtures use `testing::no_automatic_snapshots`. Concurrency is asserted only as an upper bound, never as a speed (the timing-flake rule).

In `crates/reef-lib/`:

1. (S) `spec::tests::jobs_env_accepts` and `jobs_env_refuses`: each denylisted name, a bad pattern, 17 names, a value other than `{share}`/`{jobs}`, `jobserver` a non-boolean; the error names the key.

In `crates/reef-daemon/` (unit):

2. (S) `build::tests::jobs_auto`: (8 cores, 16 GiB) → 8, (12, 32) → 12, (4, 16) → 4, (10, 16) → 8, (2, 1) → 1; `jobs = 5` wins; the cgroup `memory.max` parser.
3. (S) `build::tests::share`: `max(2, jobs / builders)` across builders 1 to 9, and the asking workspace counted once.
4. (M) `build::jobserver::create_reuse_replace`: a fresh FIFO is `0600` in a `0700` directory with `jobs − 1` tokens; a second start reuses it and tops it up; a regular file, a FIFO with group bits, and a symlink at the path are moved aside and replaced.
5. (M) `build::jobserver::top_up_and_drain`: read three tokens through a test client and close it: topped up within two checks (the check interval lowered through a test hook); with the client still open: not topped up; write two extra: drained; counters match.
6. (S) `build::jobserver::unlink_at_stop`: the path is gone after stop; a descriptor opened before still reads the old pool.
7. (M) `build::holders::finds_self_and_children` (macOS and Linux): a child process holding the FIFO is listed, the daemon itself is not, a closed descriptor is not.
8. (S) `build::env::variables`: the table's rows from fixture workspaces: `build.jobs` in `.cargo/config.toml` → no `CARGO_MAKEFLAGS`, no `CARGO_BUILD_JOBS`, reason `CARGO_JOBS_SET`; `[env] RUST_TEST_THREADS` → none; `test-threads` in `.config/nextest.toml` → no `NEXTEST_TEST_THREADS`; a go environment file with `GOFLAGS=-trimpath` → `-trimpath -p=<share>`; `GOENV=off`; `make_jobserver`; `jobs_env`.
9. (S) `build::env::make_hazard`: a `Cargo.lock` with `cmake` and a fake `make` printing 3.81 → `MAKE_TOO_OLD` with the note; a fake `gmake` printing 4.4.1 → a jobserver; `[build] jobserver = false` → `REPOSITORY`.
10. (S) `build::cargo_config::*`: written under a fake `~/.reef` only with `workspace_base`; rewritten on a share change; keys omitted for a `$CARGO_HOME` config setting them; a file without the header untouched; removed when turned off; `0600`. With the file active and carrying the key, a workspace under it gets no `RUST_TEST_THREADS` variable (the amendment); with the key omitted, or a workspace outside `~/.reef`, it gets the share.
11. (S) `config::tests::build_section`: defaults, each key, `jobs` bounds, `nice` bounds, unknown keys refused, `config show` sources.

In `crates/reef-daemon/tests/build_limit.rs` (through `TestDaemon`):

12. (M) `get_build_environment`: the response for a workspace (variables in order, `builders` counting a heartbeating second workspace), for none, with `limit = false` (`enabled` false, no FIFO made), through the remote listener (`REMOTE`, no variables), for a released workspace (`WORKSPACE_NOT_FOUND`).
13. (M) `daemon_status_build`: the `build` object, `available` back to the pool after a test client returns its tokens.
14. (L) `bounds_rustc`: a generated 8-crate workspace, no dependencies, `--offline`, its own target directory, and a `RUSTC_WRAPPER` that marks a file per running rustc, records the largest count, and sleeps 200 ms; `[build] jobs = 3`. One build through the RPC's variables: at most 3 at once and every token back; two at once: at most 4 (two implicit tokens). The cheap twin of J1 and J6.
15. (L) `recovers_after_kill`: the same build, its process group sent `SIGKILL` mid-build: `available` is below the pool, then back at it within two checks once nothing holds the FIFO; `top_ups` is 1.
16. (M) `store_build_under_the_limit`: a store build command that prints its environment shows the variables and `[store.env]`'s value winning, and runs at the configured niceness.

In `crates/reef-vcs/` (`tests/ws_run.rs`, `tests/provider.rs`, `tests/cli.rs`, `tests/schema.rs`, `tests/mcp.rs`):

17. (M) `ws_run_build_env`: the command sees `CARGO_MAKEFLAGS`, the counts and niceness 10 (`ps -o nice` is unavailable under some sandboxes, so the command reads `getpriority` through a helper binary or `/proc`); `--env RUST_TEST_THREADS=7` and an inherited value win; `GOFLAGS` merged; `--no-build-limit` sets nothing and niceness stays; against `old_daemon` (revision 30): one note, the command runs, niceness 10.
18. (S) `provider_env`: `create` and `attach` print `env`; `contract` is 1; validated against `reef schema`.
19. (S) `ws_env`: text is `export NAME='value'` lines that `sh -c 'eval …; env'` round-trips (a value with a quote), JSON is an object; outside a workspace without an argument: `not_found` with `next`.
20. (S) `daemon_status`: the `build` line and object; the schema test; `mcp::tools` lists `GetBuildEnvironment` in `NOT_TOOLS`; the digest at revision 31.

In `crates/reef-testing/`:

21. (S) `fleet::report`: G8's halves from a canned `both` report (pass, fail at 1.11×, `n/a` without `both`), the lone ratio from three solo runs' median; the `build` samples render.
22. (M) the fleet smoke (`tests/fleet_smoke.rs`) with `--limit both`: both phases run, the report has both halves, the `build` samples are present.

## Implementation notes

- **Order**: the FIFO and its checks (tests 4 to 7); the environment (8, 9) and configuration (11, 1); the RPC and status (12, 13); `ws run`, the provider and `ws env` (17 to 20); the real builds (14 to 16); the cargo file (10); the harness (21, 22); the VM run; documents and ADR 0074.
- **Unsafe code**: `FIONREAD` (`libc::ioctl`), `proc_*` on macOS and `setpriority` in `pre_exec`, each in one small function with a `SAFETY` comment, as `holder.rs` does. `mkfifo` through `nix::unistd::mkfifo`. Verify that `libc` 0.2.191 exposes `proc_pidfdinfo` and the vnode info structure on macOS; if not, declare the one function and structure locally, as `holder.rs` does with `proc_pidinfo`.
- **Threads**: the 5 s check and the holder scan run on the blocking pool; the scan only when the pool has been short for two checks, at most every 5 s, with a budget of 50 ms at 1,000 processes (measured in the PR). `GetBuildEnvironment` never scans.
- **The workspace's files** are read from its directory (the agent may have edited `.cargo/config.toml` since the create), each under 1 MiB or treated as absent, cached by path, mtime and size; `Cargo.lock` is scanned line by line for `name = "<package>"`.
- **Linux**: the holder scan, test 7 and tests 14 and 15 run in the privileged container before pushing (macOS never compiles `cfg(target_os = "linux")` code). Verify there that Ubuntu 24.04's make 4.3 rejects J15's string, and record it in `docs/build-limit.md`.
- **Builds** of Reef itself use `CARGO_INCREMENTAL=0` and `CARGO_PROFILE_DEV_DEBUG=line-tables-only`; tests 14 and 15 build their fixtures with the fixture's defaults in a temporary target directory.
- **Dogfood**: once installed, this repository's own tickets run under the limit; run the ticket's own `cargo nextest run --workspace` through `reef ws run` with five agents' worth of load if convenient, and log friction in the PR (`-j` warnings, sandbox denials, the cargo file).

## Performance expectations, and the resource criterion

| Measurement | Expect |
| --- | --- |
| `GetBuildEnvironment` | < 5 ms warm (cached files), < 20 ms cold |
| `ws run`'s start | < 10 ms more than today |
| The 5 s check | < 100 µs (one `FIONREAD`) |
| The holder scan | < 50 ms at 1,000 processes, only while the pool is short |
| The daemon's own cost | one FIFO, two descriptors, no thread of its own (a task on the runtime) |
| A lone build | within 10% of `--no-build-limit` (J26: within noise) |
| Five tickets, standard VM, tokio | G5 to G8 pass; makespan with the limit ≤ 1.1× without (J27: 1.05× on the dev host) |

The resource criterion (#264's budget) is G5 to G8 on the standard profile, measured by the harness with `--limit both`, the numbers in the PR and in `docs/fleet-benchmark.md`.

## Acceptance criteria

- [ ] A `ws run` command's cargo draws from one machine-wide pool: at most `jobs` jobs alone and `jobs + builders − 1` for several builds, every token back at the end (tests 14, 17).
- [ ] A killed build's tokens come back once nothing holds the FIFO (test 15); a surplus is drained (test 5); a daemon restart rescues a starving build (test 4).
- [ ] Job counts are set for tools without a jobserver and never override a variable or a repository's or user's own setting (tests 8, 17); `GOFLAGS` keeps the user's flags (J23, test 8).
- [ ] A repository whose build hands the jobserver to an old make still builds (test 9).
- [ ] The provider's `env`, `reef ws env` and the cargo file reach commands `ws run` did not start (tests 10, 18, 19).
- [ ] `ws run` commands and store builds run at `nice` 10; `--no-build-limit` and `[build] limit = false` turn everything off (tests 16, 17, 12).
- [ ] On the standard profile, five tokio tickets pass G5 to G8, G9 unchanged, and a lone tokio ticket is within 10% of unlimited (median of three); the Linux profile's tokio row passes G5 to G8 (`fleet.yml`). Results in `docs/fleet-benchmark.md`.
- [ ] ADR 0074 and S-188 to S-190 written; `just ci`, `just doc`, `just api` pass.

## Verification

```sh
export CARGO_TARGET_DIR=/Users/arsood/Projects/ReefVCS/target-271 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only
cargo nextest run -p reef-lib spec
cargo nextest run -p reef-daemon build:: config::tests::build_section
cargo nextest run -p reef-daemon --test build_limit
cargo nextest run -p reef-vcs --test ws_run --test provider --test cli --test schema --test mcp
cargo nextest run -p reef-vcs --test fleet_smoke
# By hand, after installing:
reef daemon status                          # the build line
reef ws run -- sh -c 'env | grep -E "MAKEFLAGS|_JOBS|THREADS|GOFLAGS"; ps -o nice= -p $$'
reef ws run --no-build-limit -- env | grep -c MAKEFLAGS   # 0
# The gate (VM; host free space checked first):
reef-fleet run --repo tokio --profile standard --limit both --solo-runs 3 --out ~/fleet/271-tokio
reef-fleet report ~/fleet/271-tokio --gate
gh workflow run fleet.yml --ref <branch>    # with --limit both
just api && just api-breaking /Users/arsood/Projects/ReefVCS/reef/.git#branch=main
just doc && just ci
```

## Out of scope

- The executor's sandbox (R3.06) drawing from the pool: it needs the FIFO allowed in its profiles; a follow-up issue.
- cgroups or any enforcement on Linux (ADR 0073's follow-up).
- A pool that shrinks per builder (decision 4's alternative), a holder-bounded partial top-up while builds run (decision 5's alternative), and a web UI view of the pool.
- Adapters applying `env`: per program, when a program offers a way; the contract carries it now.
- Fixing the upstream crates (issues filed, not patches).

## Risks

- **The make hazard is wider than `cmake`.** Any build script that hands `CARGO_MAKEFLAGS` to an old make fails under the `fifo:` form. Mitigated: the detection list, `[build] jobserver = false`, the note naming the fix, upstream issues, and `docs/build-limit.md`; a missed package shows as J15's error, which names `--jobserver-fds`, and the doc names it too.
- **Agent sandboxes** that deny writes outside the workspace cannot open the FIFO read-write: cargo warns on every command and runs unlimited. Mitigated: documented with the path to allow; the counts and the cargo file still apply.
- **`cargo -j N` under `ws run`** prints J4's warning and is ignored. Mitigated: documented with `CARGO_MAKEFLAGS=`; the warning names the jobserver.
- **The divisor**: on a 10-core, 16 GiB machine `jobs` is 8, so a lone build loses 20% of its cores and may miss the 10% criterion there (the standard machine's 8 and 16 make them equal). The standard run calibrates it; `jobs = <n>` overrides.
- **Load the jobserver does not see**: macOS's linker is multi-threaded and test binaries spawn their own threads; five tokio tickets link many test binaries. If G5 still fails, the next lever is test threads (already at the share) and then a lower `jobs`, not a build setting.
- **The VM's disk**: the tokio run grows the sparse image by about 40 GB on a host with 149 GiB free; clean first, delete the VM after.

## Choices this brief made, and why

- **The `fifo:` form, guarded**, because only a path survives the Python and Node processes between `ws run` and cargo (J17), and the one case it breaks (J15, J16) can be detected.
- **One FIFO reused, not generations**, because commands open the path at every start and live environments name it; a reused FIFO also rescues builds that started while the daemon was down (J8).
- **An exact holder scan plus a surplus drain**, because token loss is the normal case for killed builds (J9), and overcount corrects itself while undercount never does.
- **The pool at `jobs − 1`**, because every client brings an implicit token (J6) and a lone build should get exactly `jobs`.
- **Respecting settings already made**, in files as well as the environment, because the environment overrides cargo's `[env]`, a jobserver overrides `build.jobs`, and `GOFLAGS` replaces the user's go settings (J3, J20, J23).
- **One cargo file at `~/.reef`**, because it covers every repository's workspaces with one write, and `[env]` reaches test binaries (J20) where it cannot carry the jobserver (J18).

## What later tickets inherit

- **#273 (D)**: clone-store builds run under the pool and at `nice` 10 through `store/build.rs`.
- **#272 (C) and #274 (E)**: no dependency; `reef daemon status` gains their objects beside `build`.
- **RX.06** gates G5 to G8 with `--limit both`.
- **R3.20** can read `GetBuildEnvironment`'s `jobs` and `share` when it budgets checks.
- **The executor follow-up** passes the FIFO into its sandbox and its job environment.

## Departures from ADR 0073

Recorded in ADR 0074; ADR 0073's text stays as written, with "Amended by 0074" in its header.

- The FIFO's path is stable (`<state>/build/jobserver`) and reused across restarts, not `jobserver-<generation>`; lost tokens come back by an exact holder scan and at start, with surplus drained, instead of a new FIFO after a minute idle.
- The pool holds `jobs − 1` tokens.
- `CARGO_MAKEFLAGS` is withheld where a dependency hands it to make older than 4.4, or `build.jobs` is set; variables set in the workspace's or the user's configuration files are respected, not only those in the environment; `GOFLAGS` is merged with the user's go settings.
- Under `ws run`, `CARGO_BUILD_JOBS` is the share (for build scripts' `NUM_JOBS`), not `jobs`: an inherited jobserver ignores it either way (J3).
- The cargo file is `~/.reef/.cargo/config.toml`, one for every repository, not `~/.reef/<repo-short>/.cargo/config.toml`, and carries `[env] RUST_TEST_THREADS` beside `build.jobs`; where it carries it, commands get `RUST_TEST_THREADS` from the file (adaptive), not the environment (fixed).
- Numbers: API revision 31 (planned 32), ADR 0074, S-188 to S-190 (planned S-193 to S-195), schema unchanged.

No departure from `dev-phases.md`: #271's row already describes this ticket, and it turns on G5 to G8 as the row says.

## Decisions for the user

**Answered 2026-11-12: all sixteen accepted as recommended**, with one amendment to decisions 6 and 8 (proposed by the implementer, accepted by the user): where the cargo file is active and carries `RUST_TEST_THREADS`, the variable comes from the file and is not set in a command's environment, so a long-lived session's test threads follow the current share. Applied in the variable table, the cargo file section and the departures above. The recommendations as they were put:

**Group 1: scope**

1. **Size and scope.** M: the jobserver, counts, `nice`, the RPC and status field, `ws run`, the provider's `env`, `reef ws env`, the cargo file, store builds under the pool, the harness's `--limit both`. *Recommended*: as stated (each part is small and they share one environment function). Alternatives: drop `reef ws env` and the provider's `env` (S at its top end; sessions `ws run` did not start keep only the cargo file); add the executor's sandbox now.

**Group 2: the jobserver**

2. **How cargo gets it.** The `fifo:` form in `CARGO_MAKEFLAGS`. *Recommended*: as stated, because only a path survives Python and Node intermediaries (J17). Alternatives: the fd form under `ws run` (make 3.81 understands it, but every agent tool running through Python or Node loses it with a warning, J17); both, fd when `ws run`'s command is cargo itself (two code paths for one rare case).
3. **The make hazard.** Withhold the jobserver for a workspace whose root `Cargo.lock` has a package that hands it to make (`cmake`, plus those verified at implementation) when no GNU make 4.4 or later is on `PATH`, keep the counts, note the fix; `[build] jobserver = false` in `reef.toml`; two upstream issues. *Recommended*: as stated, because Reef must not break a build to bound it (J15, J16). Alternatives: document only (such repositories fail to build on macOS until they opt out); withhold the jobserver on every machine without GNU make 4.4 (most Macs would get counts only).
4. **Pool size.** `jobs − 1` tokens: a lone build gets `jobs`, five get at most `jobs + 4`. *Recommended*: as stated (J6, J26, J27). Alternatives: the daemon withholds one token per extra builder (at most `jobs` in total, but a lone build among four idle held sessions gets `jobs − 4`); `jobs` tokens (a lone build gets `jobs + 1`).
5. **Recovery.** One FIFO at a stable path, reused and topped up at start, unlinked at a graceful stop; topped up when no process holds it (an exact scan, every 5 s while short); surplus drained. *Recommended*: as stated, because a killed build loses all its tokens (J9) and agents kill builds often. Alternatives: ADR 0073's new FIFO after a minute with no `ws run` and no holder (never fires while an agent runs as a `ws run` command, and strands environments naming the old path); also a partial top-up while builds run, bounded by the holders' child and thread counts (recovers sooner, more code and a looser guarantee).

**Group 3: what commands get**

6. **Respecting existing settings.** Skip a variable when the command's environment has it, and also when the workspace's or the user's configuration sets the same thing (`build.jobs`, cargo `[env] RUST_TEST_THREADS`, nextest `test-threads`); merge `GOFLAGS` with the user's go environment file. *Recommended*: as stated, because J3, J20 and J23 show the environment silently overriding those files (J23 drops `-trimpath`, which changes the build). Alternative: the environment only, as ADR 0073 wrote it (simpler; overrides repositories' serial-test settings and users' go flags).
7. **Default variables and `jobs`.** `CARGO_MAKEFLAGS`, `CARGO_BUILD_JOBS` (share), `RUST_TEST_THREADS`, `NEXTEST_TEST_THREADS`, `GOFLAGS -p`; others only through `[build] jobs_env` (restricted names, values `{share}` or `{jobs}`); `jobs = min(cores, GiB ÷ 2)`. *Recommended*: as stated. Alternatives: also `VITEST_MAX_WORKERS` by default (would likely help excalidraw's timeouts, but overrides a repository's `maxWorkers`, J24); also `GOMAXPROCS`; a divisor of 1.5 GiB per job.
8. **The cargo file.** One `~/.reef/.cargo/config.toml` with `build.jobs` and `[env] RUST_TEST_THREADS` at the share, rewritten as builders change, keys omitted where the user's `$CARGO_HOME` config sets them, only under the default root. *Recommended*: as stated. Alternatives: per repository under `~/.reef/<repo-short>/` (ADR 0073's wording; one write per repository); `build.jobs` only; no file (sessions `ws run` did not start get no limit).
9. **The provider's `env`.** The same variables as `ws run`, jobserver included, assembled against the hook's environment; contract stays 1. *Recommended*: as stated (the jobserver is the part that adapts, and its path survives daemon restarts). Alternative: counts only (no FIFO in long sessions; after a daemon crash an orphaned FIFO serializes cargo until the next start, J8, which the start then rescues).
10. **`reef ws env`.** A `help-all` command printing the variables for `eval`. *Recommended*: yes, about 60 lines, the CLI face of the RPC for shells Reef did not start. Alternative: none (the RPC and the provider only).
11. **Who is a builder.** Every workspace whose lease is heartbeating (`ws run`, provider followers, `reef mcp`), machine-wide. *Recommended*: as stated. Alternative: only workspaces with a running `ws run` command (`ACTIVITY` entries; misses sessions `ws run` did not start, so their builds are not counted in others' shares).
12. **What agents see.** Nothing new in `reef here`, the guide, the root help or `ws run`'s JSON; `reef daemon status` and `reef ws env` show it; `ws run` is silent unless the RPC fails. *Recommended*: as stated (agents learn nothing new; the token benchmark is unchanged). Alternative: a `build` object in `reef here` when the jobserver is withheld (helps an agent understand J4's warning, costs tokens in every `here`).

**Group 4: records and measurement**

13. **ADR.** ADR 0074 amending ADR 0073's decisions 9 and 10 with the experiments' corrections. *Recommended*: as stated (0073 records a decision the user accepted; the corrections deserve their own record). Alternative: edit ADR 0073 in place with an "Amended" section.
14. **Acceptance.** G5 to G8 (the issue says G6 to G8) for tokio on the standard VM and the Linux runner with `--limit both`, plus the lone ticket within 10% (median of three solo runs); the other stacks reported, not gated. *Recommended*: as stated, because ADR 0073 and `dev-phases.md` say this ticket turns on G5, the gate it most directly addresses. Alternative: the issue's G6 to G8 only.
15. **Numbers.** API revision 31, ADR 0074, threat rows S-188 to S-190, schema 17 unchanged (nothing is persisted). *Recommended*: as stated.
16. **Priority.** `nice` 10 for `ws run` commands and store builds, never raising a priority, `[build] nice` to change it. *Recommended*: as stated (J26: no cost alone). Alternatives: no `nice`; `nice` for store builds only.

## Departures in the implementation

- **The FIFO is `~/.reef/.build/jobserver` under the default workspace root**, not `<state>/build/jobserver`: cargo's jobserver client reads the path up to the first space, and the macOS state directory (`~/Library/Application Support/reef`) has one (review finding 1, reproduced). Elsewhere it stays `<state>/build/jobserver`; a path with whitespace gets `UNAVAILABLE` and a note. The FIFO and the cargo file wait for `~/.reef`, which a start never makes (S-185); the first `GetBuildEnvironment` after a create opens it.
- **The make check is per package**: `gmake` for `cmake` when there is one (CMake's Makefile generator looks for it first), `make` for the others. The probe never runs macOS's command-line-tools stub. The list is `cmake`, `openssl-src`, `autotools`, `tikv-jemalloc-sys`.
- **The amendment also covers `CARGO_BUILD_JOBS`**, and a cargo file counts only while Reef's file is in place and current (`active_keys`); it is compared against the canonical `~/.reef` too.
- **A FIFO removed or replaced while the daemon runs is remade** at the next check (`generation` counts the FIFOs this run made); a pool short for 10 minutes while processes hold the FIFO is named in `daemon status`'s note.
- **`--no-build-limit` is hidden from `--help`** (the 2,176-byte budget) and so from `--help-all`; `reef ws env` is listed in `reef ws --help`.
- **`GOFLAGS` is always sent** (an empty value when the go environment file has a `-p` or cannot be read), so a `GOFLAGS` in the command's environment still gets `-p`.
- **`ws run` falls back to `nice` 10 only for `UNIMPLEMENTED`**; any other failure runs the command without the limit.
- **Workspace files are read per request** (no mtime cache); **`--solo-runs` repeats solo ticket 1 only**; **`--limit both`'s off run skips its `ws gc`** (a pre-existing GC bug corrupted the second run's repository).
- **`jobs = "auto"` rounds memory to the nearest GiB** before halving it: Linux's `MemTotal` is below the nominal size.
