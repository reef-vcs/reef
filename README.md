# Reef

[![CI](https://github.com/reef-vcs/reef/actions/workflows/ci.yml/badge.svg)](https://github.com/reef-vcs/reef/actions/workflows/ci.yml)

**Version control for fleets of coding agents.**

Reef keeps git's content-addressed storage and replaces git's working model with one suited to many concurrent, speculative, token-constrained authors. The name is the metaphor: a durable structure built by countless small organisms.

> Status: Phase 1 (leased copy-on-write workspaces) is complete and was released as 0.1.0 (2026-10-25). Phase 2 (typed API, MCP server, and live state) is in progress, and its work so far is released as [0.2.0](https://github.com/reef-vcs/reef/releases/tag/v0.2.0) (2026-10-28), with prebuilt binaries for macOS and Linux. Start with the [quickstart](docs/quickstart.md). The CLI is not stable yet. The work is tracked in [docs/dev-tickets](docs/dev-tickets/phase-2/README.md); see [What runs today](#what-runs-today) and the roadmap below.

## Why

Git was designed for a few dozen humans making a few commits a day. Agents break every assumption behind that design.

- **Commit volume rises by orders of magnitude.** Thousands of small changes a day from dozens of concurrent writers. Anything that costs O(repository size) becomes the bottleneck.
- **Most work is speculative.** An agent tries several approaches and keeps one. Branching, snapshotting, and discarding must be nearly free.
- **The consumer of history is also an agent.** Tokens are a performance metric. A diff that costs 40k tokens to read is slow no matter how fast it computed.
- **Conflicts are routine.** Two agents editing the same file at once is the normal case. A conflict that halts an agent in a modal state halts the fleet.
- **Trust shifts from author to evidence.** No human can review every line. A change must carry verifiable evidence of which tests ran on exactly this tree.
- **Git ceremony is waste.** Staging areas, detached HEADs, and parsing porcelain output burn tokens and cause errors. Agents want a typed API.

Git worktrees, the current answer to agent isolation, make the problem concrete: every worktree is a full copy, its dependency directories dwarf the source, it has no owner and no expiry, and it holds the only copy of uncommitted work, so nobody dares delete one.

## What Reef does differently

1. **Snapshots, not commits.** The working copy is always a snapshot. No staging area, no uncommitted state.
2. **Stable identity, mutable content.** A change has a permanent ID that survives rebase and amendment.
3. **Conflicts are first-class objects.** A conflicted merge still produces a valid snapshot. It can be stored, delegated to a resolver, or fixed later. Nothing blocks.
4. **Everything is undoable.** A global operation log records every mutation, so any agent action reverts in one call (`reef undo <op>`).
5. **Workspaces are caches, not truth.** Working directories are leased, copy-on-write, and reclaim themselves when the agent goes away. Deleting one loses nothing.
6. **Hub and ephemeral workspaces.** One authoritative store per repository with lazy-fetching clients. Full clones are the exception.
7. **Evidence rides with the change.** Test results and build hashes are signed, content-addressed objects linked to the snapshot they were computed on. Landing is gated on evidence, not on who pushed.
8. **API first, CLI second.** A local daemon speaks a typed protocol. The CLI and an MCP server are thin clients. No agent should ever regex a log.

A **proposal** is the unit of review: a change or stack of changes, with its evidence, put forward for main. It is the change itself in its proposed state, identified by its change ID. There are no pull requests.

## What runs today

Reef works on existing git and [Jujutsu](https://github.com/jj-vcs/jj) repositories. Release 0.1.0 was Phase 1, which replaces git worktrees for agents, and 0.2.0 adds the Phase 2 work so far. Coming from 0.1, read [Upgrading from 0.1](CHANGELOG.md#upgrading-from-01) first.

### Phase 1 (0.1.0)

- **A local daemon** (`reef daemon run|start|stop|status`) serving gRPC over a Unix socket, with its state, configuration, and logs under a per-user state directory. It serves many repositories; the first `reef` command in a checkout registers it, and in a plain git checkout first creates jj's side of a colocated repository (`.jj/` plus one line in `.git/info/exclude`).
- **Leased workspaces.** `reef ws create` materializes a workspace on a change in a second or two on a 100k-file repository (under half a second on overlayfs), `reef ws run` runs a command inside a fresh workspace with the lease heartbeated for the life of the process and the workspace released on exit, `reef ws move` puts another change in a workspace's directory without giving up its lease, and `reef ws list|inspect|release` manage them. When an agent dies, its lease expires, a final snapshot lands in the store, and the directory is reclaimed. Nothing is lost.
- **Copy-on-write backends.** APFS clones on macOS, kernel overlayfs or fuse-overlayfs on Linux, reflink clones on btrfs and XFS, and a plain copy fallback everywhere, all behind one `Backend` trait and one conformance suite. A workspace costs disk proportional to what the agent changed.
- **Continuous snapshots.** A filesystem watcher (`notify`, or Watchman on large repositories) snapshots each workspace through jj-lib after every debounced write. Watcher snapshots are marked local-only and never leave the machine until a change is published.
- **Garbage collection.** `reef ws gc` reclaims operations past the undo window, base cache entries no workspace uses, and stale workspace records, and reports what it freed.
- **Backup and restore.** `reef backup <dir>` copies the daemon's database and every repository's jj state while agents keep writing, pausing each repository only while its operation files are copied; `reef restore` brings it back offline, and the next start rebuilds every workspace's directory from its last snapshot without reclaiming any ([ADR 0020](docs/adr/0020-local-backup-format.md)). A quiesce RPC holds the daemon still for a filesystem snapshot instead.
- **Shared dependency stores.** A `reef.toml` in the repository declares stores such as `node_modules` or `target`; `reef store build` builds one in a scratch workspace and seals it, and the daemon mounts it read-only into every workspace after verifying it against the lockfile hash.
- **A CLI an agent can learn from.** `reef` alone prints a one-screen guide, `reef here` says which workspace you are in and what to run next, every error with a known fix carries it as a `hint` and a `next` command, and no command ever waits on input ([ADR 0025](docs/adr/0025-agent-facing-cli-contract.md)).
- **Benchmarks** for snapshot cost and workspace creation on synthetic repositories of 1k to 100k files, published from CI on both platforms. See [docs/benchmarks.md](docs/benchmarks.md) for the numbers against the Phase 1 exit criteria.
- **An onboarding benchmark.** A real Claude Code agent that has never seen Reef, given only `reef` on `PATH`, after `reef init`, with Reef not yet installed, and inside `reef ws run`: tool calls, failed `reef` calls, tokens, and cost to a working workspace, three trials each, run in CI on demand. See [docs/onboarding-benchmark.md](docs/onboarding-benchmark.md).

### Phase 2 so far (0.2.0)

- **A typed API and a thin CLI.** The daemon serves `reef.api.v1`, a versioned protobuf schema checked by `buf` lint and breaking-change checks in CI ([ADR 0029](docs/adr/0029-daemon-api-v1.md)). The `reef` binary is a client of it with no jj-lib of its own ([ADR 0033](docs/adr/0033-thin-client.md)).
- **Changes without jj.** `reef status`, `log`, `show`, `diff`, `describe`, `abandon`, `undo`, `op log`, `op show`, `ws snapshot`, `file read` and `file list` look at and revert what agents did, from a workspace or the checkout, with JSON for each and a `next` command on every error with a known fix ([ADR 0034](docs/adr/0034-cli-over-the-vcs-services.md)). The API also reads files, lists directories, and applies edits at any revision without a workspace directory ([ADR 0032](docs/adr/0032-files-without-a-workspace.md)).
- **An MCP server.** `reef mcp` serves the API to an agent harness over stdio as twenty-one tools named after its RPCs: workspaces, status, the log, changes, diff, files (read, list and edit, without a directory), describe, abandon, the operation log, undo, which changes touched a file or symbol, and a tool that waits for other agents' operations. `reef init` writes its `.mcp.json` entry for Claude Code ([ADR 0035](docs/adr/0035-mcp-server.md)).
- **Live operations.** `SubscribeOperations` streams the operation log as operations are recorded, filtered by repository, change, workspace, or agent, and resumes from a cursor; `reef op watch` prints it ([ADR 0036](docs/adr/0036-operation-log-subscriptions.md)). A broker reads each operation once and fans it out ([ADR 0038](docs/adr/0038-subscription-broker.md)): with 500 subscribers at 1,000 operations a minute, p99 delivery is 53 ms for operations and 57 ms for presence on a 12-core Mac ([docs/load-tests.md](docs/load-tests.md)).
- **Presence.** What each agent is doing now: its activity, the files it is editing, lease heartbeats, and the progress of workspace creates and store builds, never persisted ([ADR 0037](docs/adr/0037-presence-channel.md)). `reef ws run` and `reef mcp` report it on their own, `reef presence report` and `clear` report finer activity, `reef presence watch` shows it, and `reef ws renew` renews a lease held outside `ws run`.
- **A symbol index.** The daemon extracts definitions and references with tree-sitter for Python, TypeScript and JavaScript, Go, and Rust, in the background for new snapshots and edits, and accepts uploaded SCIP indexes; `reef symbols index`, `upload` and `list` drive it ([ADR 0042](docs/adr/0042-symbol-index.md)). Symbol-level diffs build on it next.
- **A local web UI.** `reef ui` opens a read-only page in your browser, live from the operation and presence streams: every repository with its latest operation, each repository's smartlog, and its workspaces and agents, with who is on which change, each lease's health, and disk use ([ADR 0047](docs/adr/0047-web-ui-transport-and-local-listener.md)).
- **Garbage collection on a timer.** The daemon runs `reef ws gc` by itself every `[gc] interval` (a day by default), and GC leaves the user's own jj workspaces loadable ([ADR 0040](docs/adr/0040-gc-and-the-users-jj-workspaces.md), [ADR 0041](docs/adr/0041-gc-on-a-timer.md)).

Still to come in Phase 2: Python and TypeScript SDKs, harness packages, and a workspace provider contract. The rows are in [docs/dev-phases.md](docs/dev-phases.md#phase-2-typed-api-mcp-server-and-live-state).

## Install

**Ask your agent.** In the repository you want to use Reef in, give your coding agent (Claude Code, Codex, Cursor, or any agent with a shell) this prompt:

```text
Install Reef from https://github.com/reef-vcs/reef, then run `reef init` in this repository and walk me through anything I still need to do.
```

The agent finds the installer below and runs it. `reef init` then registers the repository, which adds `.jj/` beside git and starts Reef's daemon, and writes Reef's instructions to `AGENTS.md`, plus the files of each agent that reads its own instead: those the repository already has files for, and the agent running `reef init` when Reef recognizes it. Later agent sessions then start in a workspace without being told. In the [onboarding benchmark](docs/onboarding-benchmark.md), Claude Code agents that had never seen Reef, given only that link, installed it and finished a task in a workspace in a median of five or six tool calls (measured before 0.1.0, against a local copy of the installer).

**Or install it yourself.** Prebuilt binaries of the latest release, 0.2.0, are available for macOS and Linux on arm64 and x86_64. The Linux binaries are static, so they run in any container.

```sh
curl -fsSL https://raw.githubusercontent.com/reef-vcs/reef/main/install.sh | sh   # ~/.local/bin, no sudo
brew install reef-vcs/tap/reef
cargo binstall --git https://github.com/reef-vcs/reef reef-vcs reef-daemon
```

The installer checks the tarball against the release's `SHA256SUMS` and never prompts. The binaries are not signed or notarized yet (R4.17). The [quickstart](docs/quickstart.md#install) has the installer's options, the system requirements, and how to verify a tarball's provenance.

To build from source, with Rust 1.97.1 or later: `cargo install --locked --git https://github.com/reef-vcs/reef reef-vcs reef-daemon` installs `reef` and `reef-daemon`, the daemon it runs. It builds `main`, which can be ahead of the release; `reef --version` names the commit. From a checkout, `cargo build --release` builds both into `target/release`. A build from source has no web UI unless you run `just web-build` (Node 24) first; `reef ui` says so. Coming from 0.1, read [Upgrading from 0.1](CHANGELOG.md#upgrading-from-01) first.

Once installed, in a git or jj repository, `reef ws run -- <your agent command>` runs an agent in a fresh workspace. `reef` alone prints the [agent guide](docs/agent-guide.md), `reef here` says which workspace you are in and what to do next, and `reef --help` lists the rest. `reef init --check` in CI keeps the files `reef init` wrote current. Output is brief JSON when stdout is not a terminal (`-o json` for every field), and an error with a known fix carries it as `hint` and `next`, a command to run.

## Roadmap

Reef builds on Jujutsu's data model and Rust crates rather than starting clean. Each phase ships something independently useful.

1. **Leased copy-on-write workspaces** (complete, released as 0.1.0). A daemon that creates workspaces via overlayfs or APFS clones, snapshots on a filesystem watcher, heartbeats leases, and reclaims expired workspaces with a final snapshot. Shared dependency store mounting. Ships as a standalone tool against existing git and jj repositories. This alone removes the worktree problem.
2. **Typed API, MCP server, and live state** (in progress; the work so far released as 0.2.0). Structured status, snapshot, diff, log, and undo over a local socket, with the CLI as a client. Live operation and presence streams. Symbol-level diffs for the first supported languages. A local web UI showing the fleet live.
3. **Evidence objects and landing gate.** The attestation format, trusted attestor policy, and a landing service that batches rebased proposals, runs checks on the batch head, and bisects on failure. The triage inbox ships here as a web UI, then as a desktop app and as iOS and Android apps, so a human can steer and approve agents from anywhere.
4. **Shared lazy object store.** A store server speaking the native protocol, with lazy fetch into the local cache and federation between stores.
5. **Structural merge.** Tree-sitter based merge, starting from [Mergiraf](https://mergiraf.org), for Python, TypeScript, Go, and Rust.
6. **Protocol specification and conformance suite.** Published once the first four phases have stabilized, so alternative implementations can exist.

## Scope

Reef targets the open ecosystem, not one organization's monorepo. The hub is a protocol anyone can host, trust is declared per repository, and git interop is the adoption path: one person on a team should be able to use Reef against an ordinary GitHub repository nobody else has heard of.

Non-goals: replacing GitHub or any review platform, running builds or tests, or being a hosted service.

## Repository layout

- `crates/reef-error`: the error conventions every crate shares: error kinds, classification, and advice.
- `crates/reef-lib`: core library. Data model, jj-lib embedding, snapshots, operation log, undo.
- `crates/reef-api`: the API schema, `reef.api.v1` (`proto/reef/api/v1`), and its generated Rust clients and servers ([ADR 0029](docs/adr/0029-daemon-api-v1.md)).
- `crates/reef-client`: the thin client of the API, its stable JSON, and how to find the daemon, with no jj-lib ([ADR 0033](docs/adr/0033-thin-client.md)).
- `crates/reef-broker`: bounded in-memory rings that read a log once and fan it out to the streams that follow it, with no jj-lib ([ADR 0038](docs/adr/0038-subscription-broker.md)).
- `crates/reef-symbols`: tree-sitter tag extraction and SCIP normalization for the symbol index ([ADR 0042](docs/adr/0042-symbol-index.md)).
- `crates/reef-daemon`: the local service and the `reef-daemon` binary. Workspaces, materialization backends, watchers, leases, garbage collection, shared stores, and the gRPC services.
- `crates/reef-vcs`: the `reef` binary, built on `reef-client` alone, and an umbrella library re-exporting the API and the client.
- `crates/reef-testing`: unpublished test fixtures and the synthetic repository generator.

## Design

The full design document is at [docs/design.md](docs/design.md). The work is broken into phases and tickets in [docs/dev-phases.md](docs/dev-phases.md), and each phase's tickets have implementation briefs under [docs/dev-tickets](docs/dev-tickets/phase-2/README.md), written so that an agent can pick one up and build it.

Architecture decisions are recorded in [docs/adr](docs/adr/README.md).

Benchmarks, what they measure, and their baseline numbers are in [docs/benchmarks.md](docs/benchmarks.md); the crash, reclaim, and load tests at 200 workspaces are in [docs/load-tests.md](docs/load-tests.md).

The [quickstart](docs/quickstart.md) takes you from installing Reef to an agent's reclaimed workspace.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) for setup, workflow, and the DCO sign-off every commit needs. Open work is the Phase 2 rows in [docs/dev-phases.md](docs/dev-phases.md#phase-2-typed-api-mcp-server-and-live-state) not yet done and the issues labeled [`phase-2`](https://github.com/reef-vcs/reef/issues?q=is%3Aissue+is%3Aopen+label%3Aphase-2).

## License

[Apache 2.0](LICENSE).
