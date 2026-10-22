# Reef

[![CI](https://github.com/reef-vcs/reef/actions/workflows/ci.yml/badge.svg)](https://github.com/reef-vcs/reef/actions/workflows/ci.yml)

**Version control for fleets of coding agents.**

Reef keeps git's content-addressed storage and replaces git's working model with one suited to many concurrent, speculative, token-constrained authors. The name is the metaphor: a durable structure built by countless small organisms.

> Status: Phase 0 (foundations) is complete and Phase 1 (leased copy-on-write workspaces) is most of the way through. A daemon and `reef` CLI build from source and run today on macOS and Linux, but there is no installer or quickstart yet, and the CLI is not stable. The work is tracked in [docs/dev-tickets](docs/dev-tickets/phase-1/README.md); see [What runs today](#what-runs-today) and the roadmap below.

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
4. **Everything is undoable.** A global operation log records every mutation, so any agent action reverts in one call.
5. **Workspaces are caches, not truth.** Working directories are leased, copy-on-write, and reclaim themselves when the agent goes away. Deleting one loses nothing.
6. **Hub and ephemeral workspaces.** One authoritative store per repository with lazy-fetching clients. Full clones are the exception.
7. **Evidence rides with the change.** Test results and build hashes are signed, content-addressed objects linked to the snapshot they were computed on. Landing is gated on evidence, not on who pushed.
8. **API first, CLI second.** A local daemon speaks a typed protocol. The CLI and an MCP server are thin clients. No agent should ever regex a log.

A **proposal** is the unit of review: a change or stack of changes, with its evidence, put forward for main. It is the change itself in its proposed state, identified by its change ID. There are no pull requests.

## What runs today

Phase 1 replaces git worktrees for agents working on existing git and [Jujutsu](https://github.com/jj-vcs/jj) repositories. This is what has landed on `main`:

- **A local daemon** (`reef daemon run|start|stop|status`) serving gRPC over a Unix socket, with its state, configuration, and logs under a per-user state directory. It serves many repositories; the first `reef` command in a checkout registers it, and in a plain git checkout first creates jj's side of a colocated repository (`.jj/` plus one line in `.git/info/exclude`).
- **Leased workspaces.** `reef ws create` materializes a workspace on a change in a second or two on a 100k-file repository (under half a second on overlayfs), `reef ws run` runs a command inside a fresh workspace with the lease heartbeated for the life of the process and the workspace released on exit, and `reef ws list|inspect|release` manage them. When an agent dies, its lease expires, a final snapshot lands in the store, and the directory is reclaimed. Nothing is lost.
- **Copy-on-write backends.** APFS clones on macOS, kernel overlayfs or fuse-overlayfs on Linux, and a plain copy fallback everywhere, all behind one `Backend` trait and one conformance suite. A workspace costs disk proportional to what the agent changed.
- **Continuous snapshots.** A filesystem watcher (`notify`, or Watchman on large repositories) snapshots each workspace through jj-lib after every debounced write. Watcher snapshots are marked local-only and never leave the machine until a change is published.
- **Garbage collection.** `reef ws gc` reclaims operations past the undo window, base cache entries no workspace uses, and stale workspace records, and reports what it freed.
- **Shared dependency stores.** A `reef.toml` in the repository declares stores such as `node_modules` or `target`; `reef store build` builds one in a scratch workspace and seals it, and the daemon mounts it read-only into every workspace after verifying it against the lockfile hash.
- **A CLI an agent can learn from.** `reef` alone prints a one-screen guide, `reef here` says which workspace you are in and what to run next, every error with a known fix carries it as a `hint` and a `next` command, and no command ever waits on input ([ADR 0025](docs/adr/0025-agent-facing-cli-contract.md)).
- **Benchmarks** for snapshot cost and workspace creation on synthetic repositories of 1k to 100k files, published from CI on both platforms. See [docs/benchmarks.md](docs/benchmarks.md) for the numbers against the Phase 1 exit criteria.

Still to come in Phase 1: moving a workspace between changes, a reflink backend for btrfs and XFS, backup and restore, crash and load tests at 200 workspaces, `reef init` for writing agent configuration, prebuilt binaries with a one-line installer, and the quickstart.

To try it from source:

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
reef daemon start
cd /path/to/a/git/or/jj/repo
reef ws run -- your-agent-command
```

`reef` alone prints the agent guide ([docs/agent-guide.md](docs/agent-guide.md)), `reef here` says which workspace you are in and what to do next, and `reef --help` lists the rest. Output is JSON when stdout is not a terminal, and an error with a known fix carries it as `hint` and `next`, a command to run.

## Roadmap

Reef builds on Jujutsu's data model and Rust crates rather than starting clean. Each phase ships something independently useful.

1. **Leased copy-on-write workspaces** (in progress). A daemon that creates workspaces via overlayfs or APFS clones, snapshots on a filesystem watcher, heartbeats leases, and reclaims expired workspaces with a final snapshot. Shared dependency store mounting. Ships as a standalone tool against existing git and jj repositories. This alone removes the worktree problem.
2. **Typed API and MCP server.** Structured status, snapshot, diff, log, and undo over a local socket, with the CLI as a client. Symbol-level diffs for the first supported languages.
3. **Evidence objects and landing gate.** The attestation format, trusted attestor policy, and a landing service that batches rebased proposals, runs checks on the batch head, and bisects on failure. The triage inbox ships here as a web UI, then as a desktop app and as iOS and Android apps, so a human can steer and approve agents from anywhere.
4. **Shared lazy object store.** A store server speaking the native protocol, with lazy fetch into the local cache and federation between stores.
5. **Structural merge.** Tree-sitter based merge, starting from [Mergiraf](https://mergiraf.org), for Python, TypeScript, Go, and Rust.
6. **Protocol specification and conformance suite.** Published once the first four phases have stabilized, so alternative implementations can exist.

## Scope

Reef targets the open ecosystem, not one organization's monorepo. The hub is a protocol anyone can host, trust is declared per repository, and git interop is the adoption path: one person on a team should be able to use Reef against an ordinary GitHub repository nobody else has heard of.

Non-goals: replacing GitHub or any review platform, running builds or tests, or being a hosted service.

## Repository layout

- `crates/reef-lib`: core library. Data model, jj-lib embedding, snapshots, operation log, undo.
- `crates/reef-daemon`: the local service and the `reef-daemon` binary. Workspaces, materialization backends, watchers, leases, garbage collection, shared stores, and the gRPC API (`proto/reef/daemon/v0`).
- `crates/reef-vcs`: the `reef` binary and an umbrella library re-exporting the two above.
- `crates/reef-testing`: unpublished test fixtures and the synthetic repository generator.

## Design

The full design document is at [docs/design.md](docs/design.md). The work is broken into phases and tickets in [docs/dev-phases.md](docs/dev-phases.md), and each phase's tickets have implementation briefs under [docs/dev-tickets](docs/dev-tickets/phase-1/README.md), written so that an agent can pick one up and build it.

Architecture decisions are recorded in [docs/adr](docs/adr/README.md).

Benchmarks, what they measure, and their baseline numbers are in [docs/benchmarks.md](docs/benchmarks.md).

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) for setup, workflow, and the DCO sign-off every commit needs. Open tickets are the issues labeled [`phase-1`](https://github.com/reef-vcs/reef/issues?q=is%3Aissue+is%3Aopen+label%3Aphase-1).

## License

[Apache 2.0](LICENSE).
