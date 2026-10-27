# 0032. Files without a workspace

- Status: accepted
- Date: 2026-10-26
- Ticket: R2.04
- Amended by: [0039](0039-carrying-edits-through-a-write.md)

## Context

The design's case for a virtual API is that many tasks need no directory: review, search, analysis, and small edits can run against the store, and a directory is materialized only when something must execute (`docs/design.md`, Workspaces). R2.01 ([0029](0029-daemon-api-v1.md)) declared that API as `FileService` (`ReadFile`, `ListFiles`, `EditFiles`) and left it unimplemented. R2.02 ([0030](0030-reads-over-jj-state.md)) settled how reads meet jj state, and R2.03 ([0031](0031-writes-over-jj-state.md)) how writes meet the live workspaces they rewrite. Neither said what an edit without a working copy is, which paths it may write, or what the edited commit's workspaces see.

## Decision

1. **Reads are R2.02's reads.** `ReadFile` and `ListFiles` resolve a revision in the request's workspace's scope, snapshot first when it uses `@` (unless `skip_snapshot` or `at_operation`), and run on the read pool. A path is read through the tree, never through a symlink: `link/x` under a symlink is `PATH_NOT_FOUND`. A file returns its bytes, a symlink its target, a conflict jj's materialization with diff-style markers, as `GetDiff` reads a side. Directories and submodules are `NOT_A_FILE`. Listings are paged in jj's component order; a recursive page walks with a matcher that skips every subtree before its cursor, so a deep page costs its own entries.
2. **An edit is a jj commit**, written through 0031's five steps as a fourth write: `revision` rewrites the commit and keeps its change id (mutable and visible only), `new_on` creates a change on it (immutable parents allowed, hidden ones refused). No directory, jj workspace or working copy is created. Workspaces on a rewritten commit or its descendants are snapshotted first and take the new tree afterwards through jj's checkout, file by file; for `new_on`, those on the parent are snapshotted and none moves. One operation of kind `edit` per request, `reef: edit commit <id>` or `reef: new change on <id>`, with 0031's attributes, undoable with `UndoOperation` like any other. Nothing changed records nothing.
3. **Edits apply in order to an overlay, all or none.** Each edit sees what the earlier ones left: `delete` then `content` replaces a symlink with a file, `copy_from` then `delete` is a rename. Content is written to the store only after every edit applied, then the tree. A dry run of the same overlay runs at step 1, so a request that is wrong against the current tree records nothing, snapshots included, unless a Reef workspace sits on the commit itself: its snapshot at step 3 can change the tree the edits apply to (an edit of `@` sees the files on disk, as a read of `@` does), and step 4 decides then.
4. **Paths.** R2.02's rules (relative, `/`-separated, no empty, `.` or `..` component), no NUL, and for edits no `.git`, `.jj` or `git~1` component in any case: jj-lib refuses to check such a tree out, and git refuses them on case-insensitive filesystems. Every proper ancestor of an edited path must be a directory or absent in the overlay, so nothing is written through a symlink or under a file.
5. **Store mounts are refused, ignore rules are not applied.** A path at or under a `[[store]] mount` that the base tree's `reef.toml`, or the one the edits leave, declares is `STORE_MOUNT_PATH`, compared ignoring ASCII case as a case-insensitive filesystem would: a workspace taking that tree would write into a read-only store or skip the file under its symlinked mount, and its next snapshot would record the file deleted. `.gitignore`, `.reefignore` and `[snapshot] exclude` decide what a snapshot picks up from disk; an edit names its paths explicitly, so an ignored path is written as asked.
6. **`copy_from` is the one schema addition** (API revision 3): a file or symlink taken by id, as the overlay holds it, with no content sent. Renaming a file over the 4 MiB request limit is otherwise impossible, and through MCP a rename would cost the file's content twice in tokens. A copy followed by a delete reuses the blob, so `GetDiff` reports a rename.
7. **Limits.** A request is under gRPC's 4 MiB decode limit (larger is gRPC's own `OUT_OF_RANGE`; `Client::edit_files` refuses before sending) and at most 1,000 edits. A text replacement reads, and produces, files of at most 16 MiB, and one request's edits hold at most 64 MiB of content in memory, sent or produced (`FILE_TOO_LARGE`), so no request can grow the daemon without bound. A read returns at most 1 MiB, and the git backend reads the whole blob for every page.
8. **Errors**: `PATH_NOT_FOUND`, `NOT_A_FILE`, `NOT_A_DIRECTORY`, `TEXT_NOT_FOUND`, `TEXT_NOT_UNIQUE`, `FILE_TOO_LARGE`, `STORE_MOUNT_PATH` and `INVALID_EDIT`, plus R2.02's `INVALID_PATH`, each about one edit with `edit_index` and the path at fault (the ancestor, or the copy's source, when that is what failed).

## Consequences

- Paged reads of a large file are quadratic in its size: each page reads the whole blob. A blob cache keyed by file id is a contained follow-up if it shows.
- Editing a commit a busy workspace sits on meets 0031's window: edits that reach that workspace's disk between its snapshot and its follow diverge the change, visibly, and nothing is lost. A workspace writing every few milliseconds while its `@` is edited through the API diverges on most edits (test `edit_while_the_workspace_writes`). An agent with a directory should write its files there; the API is for agents without one, and for commits no busy workspace sits on.
- When a workspace sits on the target, a request that is wrong costs that workspace's snapshot, which stands in the log with nothing else written.
- Rewriting a commit rebases its descendants, which may become conflicted, as every jj rewrite does.
- The mount rule reads `reef.toml` from the trees, not from live workspaces' mounts: a workspace created from an older spec with a mount the current one dropped is not protected, though its snapshots still exclude that mount.
- Commit identity is the daemon's: the author stays on a rewrite, and the committer (and a new change's author) is the daemon's identity; the agent is the operation's `reef.agent`.

## Alternatives considered

**A scratch workspace per edit.** Materializing a directory is what the API exists to avoid, and its create and release would dominate a small edit.

**Applying edits to a live workspace's directory and snapshotting it.** Races the agent working there, and needs a directory.

**A delete that ignores absent paths.** A wrong path is the likelier cause, and all-or-none makes failing safe to retry.

**Following symlinks that stay inside the repository.** Whether a link stays inside depends on the tree it is resolved in; refusing every write through a link is simpler and leaves nothing to get wrong.

**Client streaming for large files.** The API has none (0029); larger content belongs in a workspace.

**A refusal that is always final at step 1.** Would make an edit of `@` blind to the files on disk, unlike every read of `@`.
