# 0011. Hash function and object backend by phase

- Status: accepted
- Date: 2026-10-15
- Ticket: R0.07

Backfilled from design.md; decision predates this record.

## Context

The [data model](../design.md#data-model) says all objects are content-addressed, and the technology table names BLAKE3 for hashing and FastCDC for chunking. Building on jj-lib ([0002](0002-build-on-jj-lib.md)) means using jj's git backend, where trees, blobs, and snapshots have git SHA-1 identities, until Reef has its own backend. The data model and [Storage](../design.md#storage) sections state this explicitly. The shared store arrives in phase four and is the first point where Reef controls the object format end to end.

## Decision

We will use different object backends by phase.

- **Phases one to three:** jj-lib's git backend, unchanged. A Reef repository is a jj repository and opens with the `jj` CLI. Identities are SHA-1.
- **Phase four:** a jj-lib `Backend` over Reef's own object format (R4.03): BLAKE3 identities, FastCDC-chunked blobs, packfile-style files on disk indexed in SQLite as the local cache, and the store protocol as the remote. It replaces the git backend for repositories that opt in or are created against a store.

Change IDs are random, not derived from content, so they survive the switch. Migrating an existing git-backed repository rewrites commit identities once and records the mapping.

## Consequences

- Nothing in phases one to three may depend on the hash algorithm or on blob chunking *(addition)*.
- Evidence objects (R3.01) bind to whatever snapshot identity the backend produces, so an evidence object references a hash qualified by its backend *(addition)*.
- The git backend's SHA-1 is a known weakness until a repository moves to the Reef backend *(addition)*.
- A repository that migrates to the Reef backend can no longer be opened by the `jj` CLI *(addition)*.

## Alternatives considered

**A Reef backend from phase one.** BLAKE3 and chunking everywhere from the start. Rejected: it puts a storage engine in front of every phase-one deliverable *(addition)*.

**Stay on the git backend forever.** No migration. Rejected: no chunking, SHA-1 identities, and the store protocol would have to speak git objects *(addition)*.

**SHA-256 git objects.** Keeps git's format with a stronger hash. Rejected: git's own transition is incomplete and jj's support is partial *(addition)*.
