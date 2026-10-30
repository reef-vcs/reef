# 0042. Symbol index

- Status: accepted
- Date: 2026-10-28
- Ticket: R2.14

## Context

The [design](../design.md) gives Reef symbol-level diffs and history queries (Query layer), picks "SCIP, tree-sitter tags as fallback" for the symbol model (Technology choices), and says the symbol index "is keyed by file content hash so unchanged files share entries across snapshots" (Scale). Its durability table calls the index local and rebuilt by re-indexing. Phase 2's exit criterion asks for symbol-level diffs in Python, TypeScript, Go and Rust, which R2.15 builds on this index. Those diffs must work in a repository where nobody has run an indexer.

[0003](0003-sqlite-for-daemon-metadata.md) puts Reef's metadata and "its indexes over the operation log and symbols" in one SQLite database per machine. [0029](0029-daemon-api-v1.md) decision 12 says every `ACCESS_WRITE` response carries the `Operation` it recorded, with three exceptions.

## Decision

1. **The daemon extracts tags itself, with its own queries.** `reef-symbols` parses Python, TypeScript, TSX, JavaScript (with the TSX grammar), Go and Rust with tree-sitter, and runs one query per language in `queries/*.scm`. Each query was written from the grammar's upstream `tags.scm` and extended. The upstream queries cannot tell `A::close` from `B::close` in Go or Rust. TypeScript's holds only TypeScript's additions to JavaScript's. A `@scope` capture names the type a Go method's receiver or a Rust `impl` belongs to. Extraction drives `QueryCursor` directly, since `tree-sitter-tags` has no such capture. `TAGS_VERSION` and each grammar's version make an entry's extractor string (`tags/1/rust/0.24.2`). Reads ask only for the current one, and GC sweeps the rest.
2. **Tags symbols use SCIP's grammar.** A definition's symbol is `reef-tags . . . ` followed by the file's path components as namespaces (the directory's, for Go, whose package is a directory), the enclosing definitions' descriptors, any scope type, and its own descriptor. A tags reference has a name and no symbol. Resolving a reference by name is R2.15's heuristic.
3. **SCIP is uploaded, never run.** `UploadScipDocuments` takes encoded `scip.Document`s, opaque to the API, each with the blake3 of the bytes its indexer read. The daemon checks that digest against the blob at the path in the revision's tree. The check is exact, and it is the only one that is: a document carries no hash of its own, so an index run before an edit would otherwise land on the edited file with plausible ranges. Positions are converted to UTF-8 byte columns. An unspecified encoding is read as UTF-16 for `scip-typescript` and `scip-python` and UTF-8 otherwise. Locals, diagnostics, documentation and relationships are dropped. The daemon vendors `scip.proto` and compiles it with prost. The `scip` crate would bring a second protobuf runtime.
4. **Entries are keyed by path, file id and source. Trees keep coverage.** One entry is a file's tags or its SCIP document, keyed by `(path, file id, source)`, so unchanged files share entries across every tree that holds them. A tree row records a pass's counts and `COMPLETE`. The row is a hint: GC can remove entries a complete tree uses, so every read looks each file's entry up and extracts a missing one on the spot. A file answers from SCIP when an upload covers its exact content, and from tags otherwise.
5. **A pass per tree, single-flight, off the repository thread.** Indexing a commit diffs its tree from its first parent's when that tree is complete, and walks the tree otherwise. Concurrent requests for one tree share one pass, which outlives them. Passes run on the blocking pool, and extraction on the index's own threads at low priority (`setpriority` per thread on Linux, the utility QoS class on macOS). Neither the repository thread nor the read pool waits on a parse. Snapshots that change a tree and `EditFiles` commits are queued in the background. The queue keeps one pending commit per workspace and up to 256 edit commits. `[symbols] background = false` turns it off, and tests do.
6. **A per-repository cache file, outside backup and migrations.** `repos/<id>/symbols.sqlite3` has its own writer and readers in WAL mode. Unlike the rest of the metadata, it is not in `reef.sqlite3`, which amends 0003 for the symbol index. The index can reach gigabytes, it is rebuilt rather than restored, and it must not share the metadata connection's mutex with lease heartbeats. It has no migrations: a file of another format, or one that is not a database, is recreated with a warning. Backup does not copy it, and forgetting the repository deletes it.
7. **`UploadScipDocuments` is a write that records no operation.** It is `ACCESS_WRITE`, because a read-only token (R2.08) must not plant data other agents read. What it stores is derived and outside jj, so it joins decision 12's exceptions, which this ADR amends. `IndexSymbols` is a read: what it writes is derived and rebuilt on demand.
8. **GC's fifth step.** Mark every entry the working-copy trees in the view use. Then sweep tree rows and entries not used since `keep_newer`, and tags entries of outdated extractors. Last, while the file passes `[symbols] max_size` (2 GiB by default), evict unmarked entries, least recently used first. Use times are milliseconds: GC marks unconditionally, and reads refresh them at most once a day. A failure is logged, and GC goes on.
9. **No MCP tool yet.** `tools/list` is at its 20,000-byte budget. R2.15 decides whether the diff tool gains a symbols flag. (It did: see [0043](0043-symbol-level-diff.md).)

## Consequences

- Symbols work in any repository with no setup, for six file types. Any other language needs an uploaded SCIP index.
- Tags are syntax, not semantics. A tags reference over-matches common names and misses calls through aliases, and a tags symbol changes when its file moves. Answers say which source they came from.
- A SCIP entry applies to every tree holding the same file at the same path. Its definitions are right there, but its references resolved against the uploaded revision's dependencies, which a later tree may have changed. Uploads per revision keep the window small.
- A tree's `scip_files` is as of its last pass or upload. An upload made at another revision that covers a shared file shows up after the tree's next pass.
- `reef-daemon` grows by the grammars' C, about 8 MB (`docs/benchmarks.md`), and `reef` does not change. `crate_graph` keeps tree-sitter and `reef-symbols` out of the client.
- C parsers run on repository content in the daemon, bounded by a size cap, a one-second parse budget per file, and no reads through symlinks (`docs/threat-model.md`).

## Alternatives considered

- **A table in `reef.sqlite3`.** One file, as 0003 says, but the index would share the mutex lease heartbeats take and grow every backup by a rebuildable cache.
- **A manifest of entries per snapshot.** It is the design's "per snapshot" taken literally, but it costs files times snapshots. jj's tree already is that map.
- **Running SCIP indexers in the daemon.** They need toolchains, dependencies and minutes of compile time, which is the executor's business (Phase 3).
- **Uploading tags.** There is no interchange format for tag output, and the daemon can extract tags itself.
- **`tree-sitter-tags` and the upstream queries.** They cannot carry a receiver or an impl's type.
- **The `scip` crate.** It pins a second protobuf runtime beside prost.
- **Client streaming for uploads.** 0029 decision 13 keeps streams server-side for gRPC-web. Independent batches of 1,000 documents do the same job.
- **Indexing on demand only.** The first name query on a large tree would then wait minutes. Background indexing keeps the live trees ready.
