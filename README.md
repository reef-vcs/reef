# Reef

[![CI](https://github.com/reef-vcs/reef/actions/workflows/ci.yml/badge.svg)](https://github.com/reef-vcs/reef/actions/workflows/ci.yml)

**Version control built for a world where AI agents write most of the code.**

Reef keeps git's content-addressed storage and replaces git's working model with one suited to many concurrent, speculative, token-constrained authors. The name is the metaphor: a durable structure built by countless small organisms.

> Status: Phase 0 (foundations) is in progress. Nothing user-facing runs yet. The work is tracked in [docs/dev-tickets](docs/dev-tickets/phase-0/README.md); see the roadmap below for what comes after.

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

## Roadmap

Reef builds on [Jujutsu](https://github.com/jj-vcs/jj)'s data model and Rust crates rather than starting clean. Each phase ships something independently useful.

1. **Leased copy-on-write workspaces.** A daemon that creates workspaces via overlayfs or APFS clones, snapshots on a filesystem watcher, heartbeats leases, and reclaims expired workspaces with a final snapshot. Shared dependency store mounting. Ships as a standalone tool against existing jj repositories. This alone removes the worktree problem.
2. **Typed API and MCP server.** Structured status, snapshot, diff, log, and undo over a local socket, with the CLI as a client. Symbol-level diffs for the first supported languages.
3. **Evidence objects and landing gate.** The attestation format, trusted attestor policy, and a landing service that batches rebased proposals, runs checks on the batch head, and bisects on failure. The triage inbox ships here as a web UI, then as a desktop app and as iOS and Android apps, so a human can steer and approve agents from anywhere.
4. **Shared lazy object store.** A store server speaking the native protocol, with lazy fetch into the local cache and federation between stores.
5. **Structural merge.** Tree-sitter based merge, starting from [Mergiraf](https://mergiraf.org), for Python, TypeScript, Go, and Rust.
6. **Protocol specification and conformance suite.** Published once the first four phases have stabilized, so alternative implementations can exist.

## Scope

Reef targets the open ecosystem, not one organization's monorepo. The hub is a protocol anyone can host, trust is declared per repository, and git interop is the adoption path: one person on a team should be able to use Reef against an ordinary GitHub repository nobody else has heard of.

Non-goals: replacing GitHub or any review platform, running builds or tests, or being a hosted service.

## Repository layout

- `crates/reef-lib`: core library. Data model, jj-lib embedding, workspaces, merge, queries.
- `crates/reef-daemon`: the local service and the `reef-daemon` binary.
- `crates/reef-vcs`: the `reef` binary and an umbrella library re-exporting the two above.
- `crates/reef-testing`: unpublished test fixtures and the synthetic repository generator.

## Design

The full design document is at [docs/design.md](docs/design.md). The work is broken into phases and tickets in [docs/dev-phases.md](docs/dev-phases.md), and the current phase's tickets have implementation briefs under [docs/dev-tickets](docs/dev-tickets/phase-0/README.md).

Architecture decisions are recorded in [docs/adr](docs/adr/README.md).

Benchmarks, what they measure, and their baseline numbers are in [docs/benchmarks.md](docs/benchmarks.md).

## Contributing

The project is at the design stage. Issues and discussion are welcome. Code contributions will make sense once phase one has a skeleton.

## License

[Apache 2.0](LICENSE).
