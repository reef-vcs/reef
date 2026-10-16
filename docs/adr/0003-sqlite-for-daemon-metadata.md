# 0003. SQLite for daemon metadata, leases, and indexes

- Status: accepted
- Date: 2026-10-15
- Ticket: R0.07

Backfilled from design.md; decision predates this record.

## Context

The daemon needs a durable, queryable local store for the workspace inventory, leases, the status index, and later the query layer's indexes over the operation log and symbols ([Technology choices](../design.md#technology-choices), [Storage](../design.md#storage)). The status index must stay consistent across concurrent agents on one machine, so a flat file will not do. One daemon runs per machine and serves every registered repository ([Architecture](../design.md#architecture)). The design's "SQLite over RocksDB for the daemon" paragraph weighs the two candidates.

## Decision

We will use SQLite for the daemon's metadata.

- One database per machine in the daemon state directory, with every table keyed by repository ID, as R1.01 specifies.
- Accessed through a Rust binding chosen in R1.01; `rusqlite` with bundled SQLite is the expected default *(addition)*.
- WAL mode, with schema migrations tracked in the crate *(addition)*.
- jj's operation log stays authoritative in the repository, in jj's format. SQLite holds Reef's own metadata and its indexes over the operation log and symbols, not a second copy of jj's operation store.

The remote store's metadata is out of scope; the design puts it in Postgres.

## Consequences

- The query layer gets SQL for free, and the database is readable from any language when debugging.
- SQLite has a single writer. The daemon is the only writer and clients go through the API, which the design requires anyway.
- Backups use SQLite's online backup API ([Durability and backup](../design.md#durability-and-backup)).
- The daemon's write rate is expected to stay well within SQLite's range. Moving the hot path to RocksDB is allowed only on profiling evidence.

## Alternatives considered

**RocksDB.** What EdenFS uses, and it scales further. Rejected: an opaque binary dependency with no SQL, so history-as-a-database would have to be built by hand.

**sled or redb.** Pure Rust embedded stores. Rejected: less mature and no SQL *(addition)*.

**Flat files.** Simplest. Rejected: not consistent across concurrent agents on the same machine.
