# 0074. The build limit: one FIFO, recovered by its holders

- Status: accepted
- Date: 2026-11-13
- Ticket: [#271](https://github.com/reef-vcs/reef/issues/271) ([brief](../dev-tickets/phase-3/issue-271-build-limit.md))
- Amends: [ADR 0073](0073-laptop-resource-budget.md), decisions 9 and 10

## Context

ADR 0073 decided the mechanism of the machine-wide build limit:

- a jobserver the daemon owns, in GNU make's protocol over a named FIFO, handed to cargo as `CARGO_MAKEFLAGS`;
- job counts, fixed at a command's start, for tools without a jobserver;
- `nice` 10 for `reef ws run` commands;
- the provider contract's `env` and a daemon-written `.cargo/config.toml` for commands Reef does not start.

#271's brief tested each part on macOS (cargo 1.99.1, nextest 0.9.148, Go 1.27.4, make 3.81; experiments J1 to J27). Most of the design held:

- cargo honours a `fifo:` jobserver from `CARGO_MAKEFLAGS`, and so do rustc's codegen threads and the `cc` crate;
- three builds on a pool of 7 tokens ran at most 10 compiles, one implicit token each;
- five builds on 7 tokens ran 12 compiles instead of 60, at 1.05× the makespan;
- a lone build at the pool's size, at `nice` 10, ran as fast as one without.

Four findings changed the design:

- **A killed build loses every token it held.** `SIGKILL` of cargo, or of its process group, lost all 7 tokens every time (J9). Agents kill builds often. ADR 0073's recovery was a new FIFO at a new path after a minute with no `ws run` and no holder. It never fires when the agent itself is the `ws run` command, and a new path strands every environment that names the old one.
- **Every client brings an implicit token** (J6), so a pool of `jobs` lets a lone build run `jobs + 1` jobs.
- **Some builds break under a `fifo:` jobserver.** Cargo hands build scripts `--jobserver-fds=fifo:<path>`, which GNU make before 4.4 rejects (J15). The `cmake` crate passes it to make on macOS exactly when the jobserver is a FIFO (J16), and `openssl-src`, `autotools` and `tikv-jemalloc-sys` pass it on every Unix. macOS's `/usr/bin/make` is 3.81. The fd form (`--jobserver-auth=R,W`) suits old make, but dies at the first Python or Node process between `ws run` and cargo (J17), which is where agents' tools run.
- **The environment overrides files.** An inherited jobserver silently lifts a `build.jobs` cap (J3); `RUST_TEST_THREADS` in the environment overrides a repository's `[env]` request for serial tests (J20); `GOFLAGS` in the environment replaces the user's `go env -w GOFLAGS=-trimpath`, which changes what is built (J23). Cargo's `[env]` cannot carry `CARGO_MAKEFLAGS` (J18).

## Decision

1. **One FIFO at a stable path without whitespace**: `~/.reef/.build/jobserver` under the default workspace root, else `<state>/build/jobserver`. Cargo's jobserver client reads the path up to the first space, and the macOS state directory, `~/Library/Application Support/reef`, has one (found in review; a state directory with whitespace gets no jobserver and a note). The FIFO is `0600` in a `0700` directory, its owner and type checked at every start, and made again at the next check if it is removed or replaced while the daemon runs. The daemon reuses it across restarts and refills it at every start. A graceful stop unlinks it, so a build started while no daemon runs gets cargo's warning and an unlimited build, not a silently serial one (J7, J8). The daemon holds it open read-write for its whole life, and counts its tokens with `FIONREAD` on a second, read-only descriptor, since macOS answers 0 on the read-write one (J12).
2. **The pool holds `jobs - 1` tokens.** A lone build runs exactly `jobs` jobs; five run at most `jobs + 4`.
3. **Recovery by the FIFO's holders.** Every 5 seconds the daemon reads the pool. When it has been short for two checks and no process but the daemon holds the FIFO (macOS `proc_pidfdinfo`, Linux `/proc/<pid>/fd`; same-uid processes only), every missing token is lost, and the daemon writes them back. A surplus is drained. Overcounting corrects itself: a surplus is temporary. Undercounting never would.
4. **The jobserver is withheld where it would break a build.** If a workspace's root `Cargo.lock` lists a package known to pass the jobserver to `make`, and the daemon's `make` is not GNU make 4.4 or later, the workspace gets the job counts and no jobserver, with a note naming the package, the `make` found and the fix. The program that matters is the one the crate's build runs: `gmake` for `cmake` when there is one (CMake's Makefile generator looks for it before `make`), `make` for the others; the fix is GNU make 4.4 or later as that program. `[build] jobserver = false` in `reef.toml` covers what the list misses. The `fifo:` form stays, because only a path survives every process tree.
5. **Settings already made are respected, in files as well as in the environment.**
   - `CARGO_MAKEFLAGS` and `CARGO_BUILD_JOBS` are left out where the workspace's or the user's cargo configuration sets `build.jobs`.
   - `RUST_TEST_THREADS` is left out where either sets it in `[env]`, and `NEXTEST_TEST_THREADS` where `.config/nextest.toml` sets `test-threads`.
   - `GOFLAGS` is merged with the go environment file's flags, not replaced.
   - Under `ws run`, `CARGO_BUILD_JOBS` is the share, not `jobs`. A jobserver ignores it either way, but it bounds `NUM_JOBS`, which build scripts that run their own `make -j` read (J13).
6. **One cargo file**, `~/.reef/.cargo/config.toml`, only under the default workspace root Reef owns, with `build.jobs` and `[env] RUST_TEST_THREADS` at the current share.
   - It is rewritten as builders come and go. Cargo reads it at every invocation, so where it carries a key, `ws run` does not set the matching variable: a long-lived session's counts then follow the current share instead of the share at its start.
   - Each key is left out when the user's `$CARGO_HOME/config.toml` sets it, since this deeper file would override theirs (J19).
   - It is never written over a file without Reef's header.

The rest of ADR 0073's decisions 9 and 10 stand:

- `share = max(2, jobs / builders)`, where builders are the workspaces whose leases are heartbeating, machine-wide;
- `jobs = min(cores, GiB / 2)`;
- `nice` 10 for `ws run` commands, and here for store builds too;
- `MAKEFLAGS` only with `[build] make_jobserver = true`;
- a repository's `[build] jobs_env` names, restricted (S-188);
- `reef ws run --no-build-limit` and `[build] limit = false`;
- no variable through the remote listener.

The surface is `WorkspaceService.GetBuildEnvironment` (READ, not an MCP tool), `GetDaemonStatusResponse.build`, `reef ws env`, the provider contract's `env` (contract 1), and API revision 31.

## Consequences

- A lone build is unchanged. Five share the machine: on the dev host, five builds ran 12 compiles instead of 60 and finished at 1.05× the unlimited makespan (J27). The standard-machine gate is G5 to G8 with `reef-fleet --limit both` ([docs/fleet-benchmark.md](../fleet-benchmark.md)).
- A repository with a `cmake`-built `-sys` crate on a Mac without GNU make 4.4 gets the counts but no jobserver until it installs one. The note says so. The two upstream fixes are for the `jobserver` crate (never emit `--jobserver-fds=` for a `fifo:` client) and the `cmake` crate (check make's version before passing a named pipe).
- An agent sandbox that denies writes outside the workspace denies opening the FIFO: cargo warns and runs unlimited, and the counts still apply. [docs/build-limit.md](../build-limit.md) names the path to allow.
- `cargo -j N` under `ws run` is ignored with a warning; `CARGO_MAKEFLAGS=` before the command is the way out.
- Load the jobserver cannot see (the linker's threads, test binaries' own threads) is bounded only by the counts. If G5 still fails on the standard machine, the next lever is a lower `jobs`, not a build setting: Reef never sets what changes a build's output.
- Threat rows S-188 to S-190.
