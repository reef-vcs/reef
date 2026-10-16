# 0002. Build on jj-lib rather than forking jj or starting clean

- Status: accepted
- Date: 2026-10-15
- Ticket: R0.07

Backfilled from design.md; decision predates this record.

## Context

Reef's [data model](../design.md#data-model) needs four things before anything agent-specific can be built: the working copy as a snapshot, stable change IDs that survive rewrites, an operation log with undo, and conflicts stored as data. Jujutsu implements all four in jj-lib, an Apache 2.0 Rust library designed to be embedded ([Technology choices](../design.md#technology-choices)).

The design's [Roadmap](../design.md#roadmap) asked whether Reef should be a contribution to Jujutsu or a separate project. Jujutsu does not do copy-on-write materialization, leases, shared dependency stores, evidence, or a typed API ([Workspaces](../design.md#workspaces)), and those are Reef's reason to exist.

## Decision

We will build Reef as a separate project that depends on `jj-lib` as a library. We will not fork `jj-cli`.

- `jj-lib` is pinned to an exact version in each Reef release *(addition)*; R0.04 sets the first pin at `=0.46.0`.
- Generic improvements, such as the gitoxide migration and Watchman support, go upstream to Jujutsu.
- Workspaces, leases, evidence, and the daemon stay in Reef.

## Consequences

- jj-lib is 0.x and breaks between minor versions, so each version bump is its own deliberate ticket, and automated dependency updates skip it *(addition)*.
- jj-lib's minimum supported Rust version becomes Reef's floor *(addition)*. For jj-lib 0.46.0 that is Rust 1.97.1.
- Reef compiles jj-lib and its dependency tree, including gix, so a clean build is slow *(addition)*.
- Through phase three Reef inherits jj's on-disk repository format, so it must open repositories created by the `jj` CLI ([0011](0011-hash-and-object-backend-by-phase.md)).
- Some hooks Reef needs may not exist in jj-lib's public API. Those are upstreamed or worked around, and R0.04 records which in ADR 0008 *(addition)*.

## Alternatives considered

**Fork jj-cli.** Gets a working tool on day one. Rejected: the CLI carries user-experience decisions aimed at humans, and a fork diverges fast and has to be kept in sync by hand.

**Build a new store on gitoxide directly.** Full control of the format. Rejected: it means rewriting change IDs, the operation log, and stored conflicts, which is years of work jj has already done *(addition)*.

**Git plumbing through libgit2.** The most familiar foundation. Rejected: git has no change identity and no operation log, and libgit2 is a C dependency the design avoids *(addition: the design rejects libgit2 for git interop, not explicitly here)*.
