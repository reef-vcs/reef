# reef-daemon

The local service of **Reef**, version control built for a world where AI agents write most of the code. It owns snapshots, workspaces, merge, and queries on behalf of agents, and hosts the typed API, workspace materialization backends, the lease manager, and filesystem watchers. It builds the `reef-daemon` binary.

The version-control data model and merge logic live in `reef-lib`, which this crate depends on. Its own model is the inventory it keeps in SQLite (repositories, workspaces, leases, reclaims), in `reef_daemon::model` and `reef_daemon::db`. It contains no CLI code.

The `reef` binary (`reef-vcs`) does not depend on this crate: it talks to the daemon through `reef-client`, which this crate re-exports as `reef_daemon::client`, and runs this crate's binary for `reef daemon run` and for the two commands that need its library but no running daemon, `reef-daemon config show` and `reef-daemon restore` ([ADR 0033](../../docs/adr/0033-thin-client.md)). This crate never depends on `reef-vcs`. The design document and roadmap are at <https://github.com/reef-vcs/reef>.

Run it in the foreground with `cargo run -p reef-daemon` (state in the platform state directory, or `REEF_STATE_DIR`); it stops cleanly on Ctrl-C or SIGTERM. The process and threading model is [ADR 0012](../../docs/adr/0012-daemon-process-model-and-threading.md); the state directory and `config.toml` are [ADR 0013](../../docs/adr/0013-daemon-state-directory-and-configuration.md), and `reef config show` prints the effective configuration. The API schema, `reef.api.v1`, is in the `reef-api` crate, compiled at build time without `protoc`; its conventions and error model are [ADR 0029](../../docs/adr/0029-daemon-api-v1.md). This crate serves it (`server`, `service`), and clients use `reef_daemon::client::Client`.

The read RPCs (`GetStatus`, `ListChanges`, `GetChange`, `GetDiff`, `ListOperations`, `GetOperation`) take the repository from its thread in one short job and do their work on the blocking pool (`reef_daemon::read::Reads`), so a slow read never delays a snapshot; the jj work itself is in `reef_lib::jj` (`revset`, `log`, `diff`, `oplog`, `effects`). [ADR 0030](../../docs/adr/0030-reads-over-jj-state.md) records how revisions, reads at an operation, pages, operation kinds and effects work. An operation the daemon did not just write is classified by its `reef.kind` attribute, else by the description Reef wrote:

| Description | Kind |
| --- | --- |
| `reef: snapshot workspace <id>`, or any snapshot in jj workspace `reef-<id>` | `SNAPSHOT` (watcher or explicit, from the snapshot index) |
| `reef: final snapshot of workspace <id> (<reason>)` | `SNAPSHOT`, `FINAL` (`SHUTDOWN` for `shutdown`) |
| `reef: move workspace <id> to <target>`, a snapshot | `SNAPSHOT`, `MOVE` |
| `reef: move workspace <id> to <target>`, with ` (move failed)` | `MOVE_WORKSPACE` |
| `reef: create workspace <id>` | `CREATE_WORKSPACE` |
| `reef: release workspace <id>`, with any parenthesized reason | `RELEASE_WORKSPACE` |
| `reef: import git refs` | `IMPORT_REFS` |
| `reef: describe commit <commit id>` | `DESCRIBE` |
| `reef: abandon commit <commit id>` | `ABANDON` |
| `reef: undo operation <operation id>`, or `undo: restore to operation <id>` | `UNDO` |
| `reef: edit commit <commit id>`, `reef: new change on <commit id>` | `EDIT` |
| any other `reef: ` description | `OTHER` |
| anything else, including jj's own `import git head` | `EXTERNAL` |

Since R2.03 every operation Reef records carries the attributes, so the descriptions matter only for older history ([ADR 0031](../../docs/adr/0031-writes-over-jj-state.md)):

| Operation | `reef.kind` | `reef.snapshot-kind` | `reef.workspace` | `reef.agent` |
| --- | --- | --- | --- | --- |
| a snapshot | `snapshot` | `watcher`, `explicit`, `final`, `move`, `shutdown` | the workspace | the request's `reef-agent`, else the owner |
| a create | `create-workspace` | | the new workspace | the request's `reef-agent`, else the owner |
| a release, reclaim, failed or interrupted create, missing directory | `release-workspace` | | the workspace | the request's `reef-agent` (release only), else the owner; none for an interrupted create |
| a move's pointer, and its roll-back | `move-workspace` | | the workspace | the request's `reef-agent`, else the owner |
| a ref import before resolving a revision | `import-refs` | | the workspace being created, moved or written for | as that request |
| `import git head` at registration, a rematerialization | `other` | | the workspace (rematerialization) | the request's `reef-agent` (registration), else the owner |
| describe, abandon, undo, edit | `describe`, `abandon`, `undo`, `edit` | | the request's workspace, if named | the request's `reef-agent`, else that workspace's owner |
| a store build's scratch create and release | as create and release | | the scratch workspace | the request's `reef-agent`, else `reef-store-build` |

`DescribeChange`, `AbandonChange` and `UndoOperation` run through `Manager::rewrite` (`workspace/rewrite.rs`): plan on the repository thread, then take the repository's create lock shared and each moved workspace's scan lock in id order (as GC does), snapshot those workspaces one at a time, descendants first (abandon and undo), write one operation, and have each moved workspace take its new commit.

`FileService` (`service/files.rs`, [ADR 0032](../../docs/adr/0032-files-without-a-workspace.md)) reads files and lists directories at a revision on the read pool, and `EditFiles` is a fourth write through `Manager::rewrite`: the edits (`reef_lib::jj::files`) are checked as a dry run when the write is planned, workspaces on the edited commit and its descendants (on the parent, for a new change) are snapshotted, the edits are applied to the tree and recorded as one commit, and workspaces on a rewritten commit take the new tree through jj's checkout. No directory is created for the edit itself.

`SubscribeOperations` (`subscribe/`, [ADR 0036](../../docs/adr/0036-operation-log-subscriptions.md)) streams the operation log. The repository thread compares its head before and after every job and tells `subscribe::OpEvents` when it moved, which covers every operation Reef records; a 200 ms poll of the op heads of followed repositories finds other `jj` writers. A subscription's position is a frontier per repository (the op heads it has everything up to), handed out as an opaque cursor; `reef_lib::jj::oplog::walk_after` reads what follows it with a bound. Each subscription is a task (`subscribe/session.rs`) that reserves its channel slot before reading the log, so a slow client is held back rather than queued for, and catches up from the log; frames hold at most 1,000 operations and 1 MiB, live ones are spaced 50 ms apart, and an empty frame every 15 s is a keepalive. GC moves caught-up subscriptions to the rewritten head and ends the rest. What one subscription reads from one repository is `subscribe::feed::read`, the seam R2.22's broker replaces.
