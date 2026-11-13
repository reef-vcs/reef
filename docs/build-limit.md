# The build limit

Five agents building at once each assume they own the machine: cargo starts one compiler per core, rustc adds threads of its own, and test harnesses run one test per core. On a standard laptop (8 cores, 16 GiB) that put the load at 25 to 45 and pushed memory into swap in [#264's baseline](fleet-benchmark.md#baseline), and tests that pass alone timed out. The daemon bounds the builds of every workspace together, machine-wide, so five builds share the machine and one build alone still gets all of it ([#271](dev-tickets/phase-3/issue-271-build-limit.md), [ADR 0074](adr/0074-build-limit-jobserver.md)).

Nothing has to be set up. `reef ws run` puts its command under the limit, and the variables it sets never override a setting the command, the repository or the user already made.

## What a command gets

`reef ws run` asks the daemon for the workspace's build environment just before it starts the command, and sets each variable the command's environment lacks:

| Variable | Value | Left out when |
| --- | --- | --- |
| `CARGO_MAKEFLAGS` | `-j --jobserver-auth=fifo:~/.reef/.build/jobserver` (the path in full) | the jobserver is withheld (below); the command already runs under a `make`'s jobserver |
| `CARGO_BUILD_JOBS` | the share | `build.jobs` is set in the workspace's `.cargo/config.toml` or in `$CARGO_HOME/config.toml`; the cargo file below carries it |
| `RUST_TEST_THREADS` | the share | the workspace's or `$CARGO_HOME`'s cargo config sets it in `[env]`; the cargo file below carries it |
| `NEXTEST_TEST_THREADS` | the share | `.config/nextest.toml` sets `test-threads` in any profile |
| `GOFLAGS` | your `go env` `GOFLAGS` plus `-p=<share>` | the go environment file already has a `-p`, or cannot be read. A `GOFLAGS` already in the environment gets ` -p=<share>` appended instead, unless it has a `-p` |
| `MAKEFLAGS` | as `CARGO_MAKEFLAGS` | unless `[build] make_jobserver = true` |
| a repository's `[build] jobs_env` names | the share, or `jobs` | already set |

- **`jobs`** is the machine's job count: the smaller of the logical cores and the memory in GiB divided by 2. On the standard laptop that is 8; on a 4-core, 16 GiB Linux runner, 4.
- **The share** is `max(2, jobs / builders)`, where the builders are the workspaces whose leases are heartbeating (a `reef ws run`, a provider follower, `reef mcp`), across every repository. Alone, a command gets 8 on the standard laptop; with five agents, 2. It is fixed when the command starts.
- **The jobserver** is the part that adapts. It is one FIFO, `~/.reef/.build/jobserver` under the default workspace root (else `<state>/build/jobserver`), holding `jobs - 1` tokens in GNU make's protocol. Cargo takes a token for every compile beyond its first, and rustc's own threads and the `cc` crate draw from it too. Every build has one implicit token, so a lone build runs `jobs` jobs and five builds together run at most `jobs + 4`.
- **Priority**: the command runs at niceness 10 (never lowered if it is already higher), so a person's terminal and editor stay responsive. A lone build loses nothing to it.

`reef daemon status` shows the limit: the jobs, the builders and the share, the tokens free, and the cargo file. `reef ws env` prints the variables for a workspace, for a shell Reef did not start: `eval "$(reef ws env -o text)"` inside it.

## Commands Reef does not start

- **The cargo file.** Under the default workspace root (`~/.reef`), the daemon keeps `~/.reef/.cargo/config.toml` with `build.jobs` and `[env] RUST_TEST_THREADS` at the current share. It rewrites the file as workspaces come and go, and cargo reads it at every invocation from any directory below. A long-lived session's cargo, and `reef mcp` sessions (a tool result cannot set a shell's environment), follow the share without a variable. Where the file carries a key, `ws run` does not set the matching variable, so the file's adapting value applies.
  - A repository's own `.cargo/config.toml` overrides it. Since it is deeper than `~/.cargo`, it would override your `$CARGO_HOME/config.toml`, so each key you set there is left out.
  - It is never written over a file that does not start with Reef's header line, and it is removed when `[build] limit` or `[build] cargo_config` is turned off.
  - A configured `[workspace] root`, or a daemon given its own state directory, gets no file.
  - Cargo's `[env]` cannot carry a jobserver, so a session the file reaches gets counts only.
- **The provider contract.** `reef ws provider create` and `attach` print an `env` object with the same variables, for an adapter that can set its session's environment ([workspace-provider.md](workspace-provider.md#create)).

## When there is no jobserver

`reef ws env -o json`'s `jobserver_skip` and `note` say why, and the job counts still apply:

- **`make_too_old`.** Cargo hands build scripts the jobserver in a form that GNU make before 4.4 rejects: make 3.81 says `make: *** internal error: invalid --jobserver-fds string 'fifo:…'`, make 4.3 `invalid --jobserver-auth string 'fifo:…'`. Some crates pass it on to `make`:
  - `cmake`: on macOS only when the jobserver is a named pipe, which it is here;
  - `openssl-src`, `autotools` and `tikv-jemalloc-sys`: on every Unix.

  macOS's `/usr/bin/make` is 3.81, and Ubuntu 24.04's is 4.3. So for a workspace whose root `Cargo.lock` lists one of those crates, the daemon offers no jobserver unless the program the crate's build runs is GNU make 4.4 or later: `gmake` for `cmake` when there is one (CMake's Makefile generator looks for it first), `make` otherwise. To get the jobserver back, install GNU make 4.4 or later (Homebrew: `brew install make` gives `gmake`; its `gnubin` directory first on `PATH` makes it `make` too). The daemon looks on its own `PATH`, which can differ from a command's. A crate missing from the list shows as that error; `[build] jobserver = false` in `reef.toml` turns the jobserver off for the repository.
- **`cargo_jobs_set`.** `build.jobs` is set in the workspace's or your cargo configuration. A jobserver would silently lift that cap, so the cap stands.
- **`repository`.** `reef.toml` says `[build] jobserver = false`.
- **`disabled`.** `[build] limit = false`.
- **`remote`.** The workspace was asked about through the remote listener. The jobserver and builders are the daemon machine's, so no variable applies.
- **`unavailable`.** The daemon could not make or keep the FIFO; the note says why.

## Where the FIFO lives

Under the default workspace root the FIFO is `~/.reef/.build/jobserver`, not in the state directory: cargo reads the jobserver's path up to the first space, and the macOS state directory, `~/Library/Application Support/reef`, has one. A daemon with its own state directory keeps it at `<state>/build/jobserver`; if that path has whitespace, there is no jobserver (`unavailable`, with a note) and the job counts still apply.

## Lost tokens

A build killed mid-job (an interrupt, a timeout, `kill -9`) loses every token it held: nothing returns them. Every 5 seconds the daemon reads how many tokens the FIFO holds. When the pool has been short for two checks and no process but the daemon has the FIFO open, every missing token is lost, and it writes them back. A surplus is drained the same way. The daemon keeps the FIFO at a stable path, reuses it across restarts, and refills it at every start, which also rescues a build that started while the daemon was down. A graceful stop removes the path: a build started while no daemon runs gets cargo's warning (`failed to connect to jobserver`) and runs unlimited. A FIFO removed or replaced while the daemon runs is made again at the next check. A process that keeps the FIFO open for good (a build server, a watcher) blocks the refill; after 10 minutes `reef daemon status`'s note names it. `reef daemon status` counts the top-ups, the tokens restored and the tokens drained.

## Opting out

- One command: `reef ws run --no-build-limit` (no variables, normal priority), or `CARGO_MAKEFLAGS=` (empty) before one cargo command, which is also the way to make `cargo -j N` count again.
  - Under an inherited jobserver, cargo ignores `-j` with a warning ("a `-j` argument was passed … ignoring"), and ignores `CARGO_BUILD_JOBS` and `build.jobs` silently.
- One repository: `[build] jobserver = false` in `reef.toml`.
- The machine: `[build] limit = false` in `config.toml`.

## Configuration

`config.toml`, read at the daemon's start:

```toml
[build]
limit = true            # the jobserver, the counts, the priority and the cargo file
jobs = "auto"           # or 1 to 1024
nice = 10               # 0 to 19; 0 for none
make_jobserver = false  # also set MAKEFLAGS (GNU make 4.4 or later as make only)
cargo_config = true     # ~/.reef/.cargo/config.toml, under the default root only
```

`reef.toml`, read from the commit a workspace starts on:

```toml
[build]
jobs_env = { VITEST_MAX_WORKERS = "{share}", CMAKE_BUILD_PARALLEL_LEVEL = "{share}" }
jobserver = true        # false where a build hands the jobserver to an old make
```

`jobs_env` accepts at most 16 names matching `[A-Z][A-Z0-9_]{0,63}`, each set to `"{share}"` or `"{jobs}"`. Names that change more than a job count are refused (S-188): the build limit's own variables, `PATH`, `HOME`, `SHELL` and the like, names starting with `REEF_`, `LD_`, `DYLD_`, `GIT_`, `SSH_`, `CARGO_` or `RUSTC`, and names ending in `FLAGS` or `_OPTIONS`. Reef sets none of these other tools' variables by default. `VITEST_MAX_WORKERS` overrides a repository's own `maxWorkers`, and `JOBS` makes node-gyp parallel where it was serial, so a repository opts in.

Store builds run under the limit too, at the same niceness; a store's `[store.env]` wins over its variables.

## What Reef recommends but never sets

The largest disk and time savings for Rust and Go builds are settings that change what a build produces, so a repository chooses them: `CARGO_INCREMENTAL=0` with `CARGO_PROFILE_DEV_DEBUG=line-tables-only` (tokio's five tickets went from 26 to 6.3 GiB), and Go's `-trimpath`. The build limit changes only how many things run at once.

## Agent sandboxes

An agent sandbox that denies writes outside the workspace also denies opening the FIFO read-write: cargo then warns `failed to connect to jobserver` on every command and runs unlimited. Allow the path `reef ws env` prints in `CARGO_MAKEFLAGS` (`~/.reef/.build/jobserver` under the default workspace root). The job counts and the cargo file apply either way.
