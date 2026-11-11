# Issues #259 and #248 `reef rebase`: bring a workspace's change onto the new trunk, and never amend a landed commit

Phase 3 · Size L, or M at its top end with the cuts in decision 1 · Fixes: issue #259 (with its comment: fetch first), issue #248 (a workspace on a landed commit amends it) · Depends on: R3.09 (lifecycle rows, observed landing), R3.13 (`Write::Land`, new working-copy commits on a landed commit, `rewrite_held`), R3.25 (conflict objects), R3.27 (assignments open on a conflicted change), #258 (the git environment, stderr classification and redaction in `reef_lib::jj::push`), and builds on R2.03 (`Manager::rewrite`, ADR 0031), ADR 0018 (staleness arms), ADR 0039 (carries), R2.02 (revsets, `trunk()`), R2.08 (remote rules), R2.13 (`reef mcp` and its budget), ADR 0025 (CLI contract, guide budget), ADR 0051 (task-first help) · Unblocks: #262's "behind trunk" state (this ticket adds the field and the hint), #261's `pull`/`fetch`/`rebase` rows, R3.10 and R3.11 (stacks rebase through the same write), R3.17 and R4.14 (the fetch path)

## Context

When `main` moves, an agent has no Reef command to bring its change onto it. `reef rebase` is an unrecognized subcommand and there is no MCP tool. In Reef's own tickets agents ran `jj git fetch` and `jj rebase` from the root checkout (PR #255, #164): a team without jj cannot, and it acts on the shared root rather than the agent's workspace. `reef ws move` starts a new change; it does not carry the current one. The issue's comment adds the fetch: Reef imports git's refs but never fetches, so "the new trunk" has to be fetched first, through the daemon, with #258's credential path.

#248 is the other side of the same mechanism. When R3.09 records a change `LANDED` because it reached `trunk()`, a workspace whose `@` is still that commit stays on it, and its next snapshot amends the landed commit; jj then moves `main` with the rewrite, past every gate. R3.13 closed this for its own landings (S-121: new working-copy commits in the `LAND` operation) and left the observed path to #248, because an observed landing writes no view change. Both issues are "a write that moves the base under live workspaces, and the workspaces following it", so one ticket builds the shared parts once.

The issue's proposal is the shape: `reef rebase [REV] [--onto REV]`, default the workspace's change onto `trunk()`; one operation, undoable; workspaces on rewritten changes get the new tree with their unsnapshotted writes carried (ADR 0039); a conflicting rebase succeeds and returns R3.25's objects with `next` `reef conflict show`; already on trunk's head records nothing and exits 0; an RPC and an MCP tool of the same shape. Its acceptance: one command from a workspace behind `main` ends on `main`'s head with the files on disk updated; the conflicting case is tested; `reef here` suggests it when the base is behind.

So this ticket builds:

1. **The fetch**: `reef_lib::jj::fetch` (jj-lib's `GitFetch` on a throwaway transaction off the repository thread, then a record on it), the daemon's `fetch` module, `GitService.FetchGitRemote` and `reef fetch` (help-all only).
2. **The rebase**: `Write::Rebase` through `Manager::rewrite`, `ChangeService.RebaseChange`, `reef rebase`, the MCP tool `rebase_change`, SDK handle methods.
3. **Following trunk (#248)**: `Write::StartOnLanded` for workspaces whose working-copy commit reached `trunk()` (in the observed landing's own operation when there is one), a carry for a working copy moved onto a new child, and a snapshot backstop that never amends a commit on `trunk()`.
4. **Discovery**: `Workspace.behind_trunk`, `reef here`'s hint, the root help, the guide, the hints for `pull`/`sync`, and the `jj rebase` advice already in Reef's own errors.

### What was verified while writing this ticket

Reef at `0d14bf1e` (#258 merged; API revision 29, `reef-api/src/lib.rs:49`; schema 17, migrations up to `0017_conflict_assignments.sql`; ADR 0070 the latest; the threat model's last row S-176; `OperationKind` ends at `OPERATION_KIND_PUBLISH = 17`). This ticket plans **API revision 30**, **ADR 0071**, threat rows **S-177 to S-184**, `OPERATION_KIND_REBASE = 18` and `OPERATION_KIND_FETCH = 19`, `Workspace.behind_trunk = 23`, `SnapshotWorkspaceResponse.started_on = 5`, and **no migration** (schema stays 17). Paths are under `crates/` unless they start with `docs/`, `web/` or `sdk/`.

- **`Manager::rewrite`** (`reef-daemon/src/workspace/rewrite.rs`) is the one path for writes that move live workspaces: (1) plan on the repository thread, listing the jj workspaces the write moves, descendants first; (2) the create lock shared, then automatic snapshots paused and scan locks taken in id order; (3) snapshot each (not for `Describe`); (4) the write and its carries in one repository-thread job; (5) follow: a workspace whose tree is unchanged gets a row update (`follow_view`), else a snapshot that finds the working copy stale and checks the new tree out, carrying late edits. `rewrite_held` takes a create lock the caller already holds (R3.13). `Write` has `Describe`, `Abandon`, `Undo`, `Land`, `Edit`.
- **Carries** (`carries()`, same file): only for a working copy moved "to another commit of the same change with another tree". ADR 0039: "Any later write through `Manager::rewrite` that moves workspaces within their change carries without new code." A rebase keeps change ids, so it carries for free. A move onto a **new** change (R3.13's `check_out` on a landed commit) gets no carry: a workspace with edits made after step 3 then meets ADR 0018's fourth arm at its next snapshot (`begin_snapshot`, `reef-lib/src/jj/snapshot.rs`: "stale with edits otherwise: plan a rewrite of `expected_commit`"), which is the landed commit. That is a narrow window in R3.13 today and the whole of #248's case, since an observed landing must not snapshot first (the snapshot is the amendment).
- **The follow's check-out** is jj's `LockedWorkingCopy::check_out` (`check_out` in `reef-lib/src/jj/snapshot.rs:1099`), which diffs the old and new trees and writes only the files that differ (`CheckoutStats`). The stale arm forces a full scan (`FullScanReason::Stale`): 146 ms on the 100k-file `large` preset, 11 ms on `medium` (`docs/benchmarks.md`).
- **R3.13's landing** (`reef-lib/src/jj/land.rs`, `land_chain`): after the rebase, every jj workspace whose working-copy commit is the landed or the original commit gets `tx.repo_mut().check_out(name, landed)` (a new empty child, as `jj new`), in the `LAND` operation. Its brief, decision 10: "#248 is not fixed here: … an observed landing writes no view change."
- **Observed landing** (`reef-daemon/src/lifecycle/observe.rs`): one task woken by every operation, 200 ms settle, returns at once for a repository with no unlanded recorded change; otherwise `on_trunk` finds recorded changes with a visible commit in `::trunk()` and `record_observed` (`lifecycle/mod.rs:1301`) writes one `TRANSITION` operation each through `record_transitions`, a transaction with no view change. It looks only at **recorded** changes: a workspace on a never-proposed commit that reaches `trunk()` (a host's merge-commit merge of a `reef publish` branch, then a fetch; a person's `jj bookmark set main -r …`) has the same hole and no transition to hang the fix on.
- **Snapshots never check immutability** (still true; R3.13's brief verified it).
- **`trunk()`** (`reef-lib/src/jj/revset.rs:94`): the first existing of local `main`, `master`, `trunk` (origin's HEAD branch first when set), then `main@origin`, `master@origin`, `trunk@origin`. `immutable_heads()` is `trunk() | tags()`. A local `main` that nobody fast-forwards stays `trunk()` while `main@origin` moves on.
- **jj-lib 0.47.0's fetch**: `git::GitFetch::new(&mut MutableRepo, GitSubprocessOptions, &GitImportOptions)`, `fetch(remote, &GitFetchRefExpression, callback, depth)` (runs `git fetch` into the private namespace `refs/jj/remote-bookmarks/<remote>/…`, `git.rs:111`, `:3420`), then `import_refs()` (`:3510`), which diffs only that namespace against the view (`diff_fetched_refs_to_import`, `:997`, private, filtered by what `fetch` recorded) and returns `GitImportStats { changed_remote_bookmarks: Vec<GitImportRefUpdate>, … }`. The import's merge (`import_refs_inner`, private) adds the new heads, merges each **tracked** remote bookmark into its local bookmark (`MutableRepo::merge_local_bookmark`, `repo.rs:1856`, public) and sets the remote bookmark (`set_remote_bookmark`, `:1873`, public). `GitFetch` holds `&mut MutableRepo` for the whole network call, so it cannot run on the repository thread without blocking it (ADR 0012: "every jj transaction … runs on this thread").
- **jj-lib's rebase**: `rewrite::move_commits(mut_repo, &MoveCommitsLocation { new_parent_ids, new_child_ids, target: MoveCommitsTarget::Roots(..) | Commits(..) }, &RebaseOptions)` (`rewrite.rs:523`) with `MoveCommitsStats` (`num_rebased_targets`, `num_rebased_descendants`, `num_skipped_rebases`, `num_abandoned_empty`); `EmptyBehavior::Keep` is the default.
- **#258's git plumbing** (`reef-lib/src/jj/push.rs`): `push_environment` (no prompts, BatchMode ssh, low-speed limits, trace off), `classify_stderr` → `GitFailure`, `redact`, `sanitize_line`, `choose_remote`, `check_remote`, `remote_names`, `core_ssh_command`; the daemon's `publish/mod.rs` has the timeout, per-branch lock and the advice for `GIT_AUTH_FAILED` and friends. `BRANCH_MOVED` and `BRANCH_CONFLICTED` hints send the agent to `jj -R <root> git fetch`; `BRANCH_DIVERGED`'s `next` is `reef show <T>` "until #259 gives a rebase".
- **`jj rebase` in Reef's own advice**: `reef-vcs/src/cli/land.rs:472` ("rebase the change yourself (`jj rebase -d {}`)") and `docs/landing.md:29`. `docs/publishing.md:156` lists #259 as out of scope.
- **Lifecycle** (R3.09): amending (or rebasing) a `PROPOSED` change keeps it `PROPOSED` with `amended: true` until the next `reef propose`; `LandChange` refuses an amended proposal (`PROPOSAL_AMENDED`, `next` `reef propose`); `QUEUED` pins the commit and R3.13 dequeues an amended one (`AMENDED`).
- **Conflicts** (R3.25, R3.27): `reef_lib::jj::conflict::read` derives a `Conflict` per conflicted commit on every read; R3.27 opens an assignment when a change's newest commit in `mutable() & conflicts()` introduces a conflict, owned by the change's latest provenance session. `reef conflict show [-r REV]` defaults to the workspace's change. `ConflictService` is in `NOT_TOOLS`.
- **The proto.** `ChangeService` has `DescribeChange`, `AbandonChange`, `TransitionChange`; `GitService` has `PublishChange`. `Workspace` ends at field 22 (`provenance`); `SnapshotWorkspaceResponse` at 4 (`scan_ms`); `Attention` at 8. Reasons `IMMUTABLE_REVISION`, `HIDDEN_REVISION`, `CHANGE_DIVERGENT`, `EXPECTED_COMMIT_MISMATCH`, `SNAPSHOT_FAILED`, `NO_DESTINATION`, `INCLUDES_WORKING_COPY` and the `GIT_*` reasons exist.
- **Remote rules** (`remote/rules.rs`): `DescribeChange` and `AbandonChange` are `Rule::None`, `MoveWorkspace` `Rule::OwnWorkspace`, `PublishChange` `Rule::SocketOnly`.
- **The CLI.** Unknown subcommands become a usage error; `ORIENTATION_GUESSES` (`cli/mod.rs:1045`) turns four guesses into a hint with `next` `reef here`, "a hint, never an alias (ADR 0025)". `reef here` reads `GetWorkspace`; `advise` (`cli/here.rs:124`) gives the in-workspace hint.
- **Budgets.** Root help 507 of 512 bytes (`cli/help.txt`). Agent guide: whole file 1,709 of 1,750 bytes; long form 1,690 of 1,700 (`cli/guide.rs:99`). `tools/list` bound 22,400, measured 22,376 at #258 (`reef-vcs/tests/mcp.rs:214`, whose comment asks a raise to be argued in the brief).
- **SDKs.** Generated clients plus hand-written workspace handles: `sdk/python/src/reef/workspace.py` (and `aio/`) and `sdk/typescript/src/workspace.ts` have `move()` (updates the handle), not `publish()`.

## Decisions

### Size, and the split (decision 1)

L. The work divides like this:

| Part | Size |
| --- | --- |
| reef-lib `jj/fetch.rs`: remote and branch choice, the throwaway fetch, the record (remote bookmarks, tracked locals except a checked-out branch), the environment and classification reused from `push.rs` | about 400 lines |
| reef-lib `jj/rebase.rs`: the set (branch and source modes), checks, destination resolution, `move_commits`, landed `@`, conflicts introduced, the new-child helper shared with `land.rs` | about 450 |
| The daemon: `Write::Rebase` and `Write::StartOnLanded` in `Manager::rewrite`, new-child carries, the `fetch` module (lock, timeout), `service/changes.rs` and `service/git.rs` handlers, `behind_trunk` | about 650 |
| #248: the trunk follower in `lifecycle/observe.rs`, `record_observed` through `Write::StartOnLanded`, the snapshot backstop in `begin_snapshot` | about 350 |
| Protos, reasons, conversions, the remote rules, `OPERATION_KIND_REBASE`/`FETCH`, undo's refusal of a fetch | about 300 |
| `reef rebase`, `reef fetch`, JSON and brief types, `reef schema` rows, help, guide, `here`, the `pull`/`sync` hints, `land.rs` advice | about 400 |
| MCP `rebase_change` and the kinds | about 100 |
| SDK handle methods (Python sync and async, TypeScript) | about 80 |
| Onboarding scenario G | about 250 |
| Web UI words and counts | about 20 |
| `docs/rebasing.md`, ADR 0071, threat model, quickstart and other pages | prose, about 450 lines |

About 3,000 lines, against #258's 2,250 at "M, top end". The cuts that bring it to M (decision 1's alternatives): drop `GitService.FetchGitRemote` and `reef fetch` (the fetch stays inside the rebase; about 150 lines), move scenario G and the SDK handle methods to follow-ups (about 330). Splitting #248 out (about 500 lines) is possible but loses the shared parts (the new-child carry, `Write::StartOnLanded`), which the rebase's landed-`@` case also uses.

What stays out, each a follow-up issue the PR files: **#202** (a descendant's directory left stale by an ancestor's **snapshot**; decision 10), **a rebase preview** (`dry_run`), **re-basing a workspace's base cache entry** after a large trunk jump (decision 13), **forwarding the client's SSH agent** (as #258), **a merge instead of a rebase** (`reef merge`).

### What a rebase is

**Rebasing** moves a set of changes onto a destination commit, keeping their change ids, and moves every workspace on a rewritten commit with them, in one operation of kind `rebase`. By default it first fetches the destination's branch from its git remote. It records no lifecycle transition.

For request revision `REV` (default `@`), mode `M` (branch by default) and destination `D`:

1. **Gate**: a caller through the remote listener never fetches (decision 9).
2. **Fetch** (unless `skip_fetch`, or no git backend, or no remote applies): the destination's branch from its remote, through `reef_lib::jj::fetch` (below), recorded in an operation of kind `fetch` when the view changed.
3. **Resolve the destination**: `onto` if given; else `trunk()` after the fetch, or its remote counterpart when that is ahead (below). None is `NO_DESTINATION` (`FAILED_PRECONDITION`, as R3.09's).
4. **Write**, through `Manager::rewrite` with `Write::Rebase`: plan (resolve `REV`, compute the set, run the checks, list the workspaces on the set and its descendants), lock, snapshot each (so the named workspace's `@` is current and no edit is lost), then on the repository thread resolve and check again and `move_commits`, then follow: each moved workspace checks out its new tree, carrying edits made since step 3 (ADR 0039, unchanged).
5. **Read conflicts** introduced by the rebase (R3.25's derivation, no hunks) for the response.

**The set** (decision 2):

| Mode | Set (`MoveCommitsTarget`) | Like |
| --- | --- | --- |
| branch (default; positional `REV`) | `Roots(roots(D..REV))`: every commit in `REV`'s ancestry that `D` lacks, from its roots, with all their descendants | `jj rebase -b REV -d D`; `git rebase D` on a branch |
| source (`--source`) | `Roots([REV])`: `REV`'s commit and its descendants | `jj rebase -s REV -d D` |

Descendants always move (jj never leaves a child on a rewritten parent), and so do the workspaces on them: this is what #202 asks for, here for the rebase's own write.

**The destination when no `onto` is given** (decision 3). After the fetch, let `T` be `trunk()` and `B` its bookmark. If `B` is local, its remote counterpart `B@R` exists on the remote that was fetched (or `origin`), and `B@R` is a strict descendant of `B`, the destination is `B@R` and a note says "rebased onto main@origin; the local bookmark main is behind it and was left as it is". Otherwise the destination is `T`. This is `git rebase origin/main` for an agent whose repository's local `main` nobody fast-forwards, which is every git user who works on branches; a jj user whose `main` tracks `main@origin` has it fast-forwarded by the fetch, so `T` is already the newest.

**Checks**, each refusing with the first offending commit, its change id and commit id in metadata:

| Check | On | Reason | `next` |
| --- | --- | --- | --- |
| `REV` resolves to one visible commit | `REV` | R2.02's revision reasons, `HIDDEN_REVISION` | |
| `D` is not in the set and not a descendant of it | `D` | `REBASE_ONTO_DESCENDANT` (`INVALID_ARGUMENT`) | |
| every commit of the set is mutable | each | `IMMUTABLE_REVISION` (`FAILED_PRECONDITION`) | none; see "A landed `@`" for `REV` itself |
| no change of the set is divergent | each | `CHANGE_DIVERGENT` | `reef log -r 'change_id(<id>)'` |
| no change of the set is `QUEUED` | each | `CHANGE_QUEUED` (`FAILED_PRECONDITION`, metadata `change_id`) | `reef op watch --kind transition --change <id>` (the hint also names `reef proposal withdraw`) |
| no commit of the set below `REV` is another active workspace's working-copy commit (branch mode; descendants are allowed) | each | `INCLUDES_WORKING_COPY` (metadata `workspace`) | `reef rebase --workspace <id>`, which moves that workspace's change and, as its descendant, this one |
| at most 1,000 commits in the set and its descendants | all | `RANGE_TOO_LARGE` (metadata `count`) | none |

`INCLUDES_WORKING_COPY` mirrors #258's S-171 rule: moving the base under another agent that is still writing changes its files without its asking. Rebasing that agent's workspace by name is the explicit form, and moves both. A commit of the set that is the **request's own** workspace's `@` is always allowed.

**Nothing to do.** When every root of the set already has `D` as its only parent, or the set is empty (`REV` is in `::D`, and `REV` is not `@`), nothing is written: `operation` is unset, `up_to_date` is true, exit 0. The fetch's operation, if the remote moved, is still reported.

**A landed `@`** (decision 4). When `REV` is `@` (or names the workspace's working-copy commit) and that commit is in `immutable()`, the workspace's change has landed (through Reef, a host's merge, or a person's bookmark move). The rebase then gives the workspace a new empty change on `D` instead (`Write::StartOnLanded`'s helper, as R3.13's landing does), records it as the rebase's operation with `started_new_change: true`, and says so: "Its change is already on main (landed); the workspace is on a new empty change on main's head." Any other immutable `REV` is `IMMUTABLE_REVISION`, hint "already on trunk; start new work with `reef ws move <id> --new-on main`".

**Conflicts are data** (decision 5). A rebase whose result conflicts is written and succeeds (exit 0): jj records the conflict in the commit, the follow checks it out with jj's markers, and the response lists one R3.25 `Conflict` for each rebased commit that **introduces** a conflict (newest first, at most 10, each with its first 20 files and no hunks; `conflict_count` counts all; descendants that only inherit one are counted in `inherited_conflict_count`, not listed). `next` is `reef conflict show -r <change>` for the first. R3.27 opens the change's assignment, owned by its session, so the agent that rebased its own change owns the resolution; the response's note says "resolve the files, then `reef propose` if it was proposed". A rebase never leaves the agent with nothing to do.

**Changes that become empty** keep their change (`EmptyBehavior::Keep`, jj's default): a change whose edits trunk already has, the usual result of a host's squash merge of a published branch, is reported in `became_empty` with the note "its edits are already on main (a squash merge?): abandon it with `reef abandon`, or keep working in it". Reef never abandons a change unasked.

**Lifecycle** (decision 6). A rebase writes no transition. A `PROPOSED` change in the set stays `PROPOSED` with `amended: true`, as for any amendment; when `REV`'s change was `PROPOSED` and the rebase was clean, `next` is `reef propose` and the note says the reviewers' view and the evidence were for the old tree. `QUEUED` is refused (above): R3.13 lands exactly the queued commit, and its landing rebases onto the destination anyway. `LANDED` changes are immutable, so they are never in the set. `WITHDRAWN`, `DECLINED` and drafts rebase freely.

**One operation, undoable.** The rebase is one operation of kind `rebase`, described `reef: rebase change <12-char id> onto <bookmark or short commit>`, with the caller's agent and `reef.workspace` as every write records them; `OperationEffects` already report the rewritten commits. `reef undo <op>` reverts it through the existing undo write, which follows and carries (ADR 0039). The fetch is its own operation (kind `fetch`), **not undoable** (`OPERATION_NOT_UNDOABLE`: "a fetch records what the remote has; fetch again"), because undoing the view's remote bookmarks would disagree with git's fetched refs, which jj-lib wrote. Undoing the rebase alone returns the change to where it was.

**Events and presence.** No new event or presence kind: the operations appear on `SubscribeOperations`, `reef op watch --kind rebase|fetch` and MCP's `wait_for_operations`; the followed workspaces' rows change as for any rewrite.

### The fetch (decision 7)

`reef_lib::jj::fetch` has #258's three-step shape, so the repository thread never waits on the network:

1. **Plan** (repository thread, create lock shared): import git refs (R1.05); choose the remote (`git_remote` if given, else the remote of `trunk()`'s bookmark when it is a remote bookmark, else #258's `choose_remote`: the only remote, else `origin`, else `GIT_REMOTE_REQUIRED`); choose the branches (`branches` if given, else `trunk()`'s bookmark name, else the remote's HEAD branch; one exact name each, checked with #258's `check_branch`). Nothing is written but the import.
2. **Fetch** (`spawn_blocking`, the per-(repository, remote) fetch lock held, not the create lock): a transaction started from the plan's `Arc<ReadonlyRepo>` inside the closure, `GitFetch::new` with `GitSubprocessOptions` carrying `push_environment` (renamed `git_environment`), `fetch(remote, {bookmark: exact names, tag: none}, callback, None)` and `import_refs()` on that transaction, then the transaction is **dropped**: git's objects and `refs/jj/remote-bookmarks/<R>/<B>` are written, the view is not. The result is `GitImportStats.changed_remote_bookmarks`.
3. **Record** (repository thread, create lock shared): a fresh transaction; for each changed remote bookmark whose view target is still the update's old target (else it was moved meanwhile: skipped with a note), add the new heads, `merge_local_bookmark` for a tracked one, and `set_remote_bookmark`; commit one operation of kind `fetch` (`reef: fetch main from origin`) when anything changed. This replays `import_refs_inner`'s merge with public jj-lib calls, about 60 lines, and is the only part that depends on jj-lib internals; `docs/rebasing.md` and ADR 0071 name it, and a test pins it against jj-lib's own import (test 3).

**A local bookmark that is git's checked-out branch** in a colocated repository (`HEAD` is the symbolic ref `refs/heads/B`, as #258 tests it) is never merged: the remote bookmark moves and the local one stays, with a note. Moving it in jj's view would let the jj CLI's next export move a person's checkout. With decision 3, the rebase still lands on the remote's head.

**Failures** reuse #258's classification and redaction with fetch reasons: `GIT_AUTH_FAILED`, `GIT_UNREACHABLE`, `GIT_NOT_FOUND`, `GIT_TOO_OLD`, and `GIT_FETCH_FAILED` (new, `UNAVAILABLE`) for the rest; `FETCH_TIMEOUT` and `FETCH_IN_PROGRESS` (`UNAVAILABLE`) as `PUBLISH_TIMEOUT`/`PUBLISH_IN_PROGRESS`; `FETCH_DISABLED` (`PERMISSION_DENIED`). A branch the remote lacks is not an error (jj-lib drops the refspec): the note says so. **A failed fetch fails the rebase** (decision 8), with `next` the same `reef rebase` plus `--no-fetch`, so an agent offline knows it is rebasing onto the last-fetched trunk; the hint says so.

**Cost.** One `git fetch` of one branch: the objects trunk gained, as `git fetch origin main` transfers. No full-repository fetch, no tags.

### Following trunk: #248 (decision 10)

Three parts, sharing one helper and the carry:

- **The helper**: `reef_lib::jj::rewrite::start_on(tx, workspaces, commit)`, `check_out` of a new empty child for each named jj workspace, extracted from `land_chain` (which then calls it).
- **The new-child carry**: `carries()` also hands a carry to a working copy moved onto a new change **whose only parent is its old commit and whose tree equals it** (`Carry { from: old, to: new, base: None }`). `carried_tree`'s merge is then the scanned tree on the new change, so edits made after the move go into the new change, never into the landed commit. This amends ADR 0039's condition and fixes R3.13's window too (an edit between `Write::Land`'s step 3 and its write).
- **`Write::StartOnLanded { commits, transitions }`** in `Manager::rewrite`: plan the active Reef workspaces whose working-copy commit is one of `commits` and is in `immutable()`; lock them; **no step-3 snapshot** (it would be the amendment); write `start_on` for each, with `transitions` in the same operation when given; carry (above); follow (tree unchanged, so `follow_view`: a row update, no disk I/O).

**The trunk follower** (`lifecycle/observe.rs`). The task already wakes on every operation and keeps each repository's `trunk()` commit. On a move it now also asks, for every active Reef workspace of the repository, whether its row's commit is in `::trunk()` (one index ancestry test each) or is a tag's target. For a recorded change found landed, `record_observed` writes its `→ LANDED` transition through `Write::StartOnLanded` with `rewrite_held` (it already holds the create lock shared and the lifecycle lock), so the new working-copy commits are **in the operation that records the landing**, as the issue asks. Workspaces on an unrecorded commit get one `StartOnLanded` operation of kind `move-workspace`, described `reef: new change on landed <short commit> for workspace <id>`, one per workspace. A repository with no active workspaces and no open proposals still returns at once.

**The snapshot backstop.** Between `trunk()` moving and the follower acting (200 ms settle plus the write), or when the follower fails, a snapshot could still amend. `begin_snapshot` therefore never plans a rewrite of a commit in `::trunk()` or targeted by a tag: when the commit it would rewrite (`rewrite_of`, in the fresh, divergent and recover arms) is one, it plans a **new commit whose parent is that commit**, with the scanned tree and a new change id, and the workspace's working copy moves to it, as the jj CLI does when a working-copy commit becomes immutable ("became immutable, so a new commit has been created on top of it"; the implementer confirms against jj-cli 0.47.1). The snapshot operation is described `reef: snapshot workspace <id> (new change on <short commit>, which is on main)`; `SnapshotWorkspaceResponse.started_on` names that commit; the row's `change_id` changes; `reef here` shows the new change. The check costs one `is_ancestor` against `trunk()`'s commit plus a lookup of tags at the commit: under 50 µs, measured (test 21).

**What the agent sees.** Its change shows `LANDED` (when recorded) and its workspace is on a new empty change on top; `reef here`'s ordinary hint applies; any edit it keeps making goes into the new change. `main` never moves by a snapshot.

**#202 is out of scope.** The rebase locks and follows every workspace it moves, descendants included, so it never leaves a descendant stale. #202's case is an **ancestor workspace's ordinary snapshot** rebasing a descendant, which runs outside `Manager::rewrite`; fixing it puts a follow on the hot snapshot path and needs #91's carry for a busy descendant. It shares `follow` and the carry, which this ticket leaves ready; #202 and #91 stay one follow-up ticket.

### The API: revision 30 (decision 11)

`changes.proto`:

```proto
// In ChangeService:
  // Moves changes onto a destination commit, keeping their change ids:
  // in branch mode every commit of `revision`'s ancestry the destination
  // lacks, in source mode `revision`'s commit, each with its descendants
  // (#259, ADR 0071; docs/rebasing.md). The destination's branch is
  // fetched from its git remote first unless `skip_fetch` (never through
  // the remote listener), and the destination defaults to `trunk()`, or to
  // its remote-tracking bookmark when that is ahead of it. Workspaces on
  // the moved commits are snapshotted first and take their new trees
  // afterwards, keeping edits made meanwhile (ADR 0039). A rebase that
  // conflicts is written and returns the conflicts it introduced. Nothing
  // to move records nothing: `operation` is unset and `up_to_date` true.
  // When `revision` is a workspace's working-copy commit that has landed
  // (is immutable), the workspace starts a new empty change on the
  // destination instead. Writes no lifecycle transition; a PROPOSED change
  // reads `amended` until proposed again. Not idempotent.
  //
  // Errors: the revision reasons; INVALID_ARGUMENT with reason
  // REVISION_REQUIRED, INVALID_COMMIT_ID or REBASE_ONTO_DESCENDANT;
  // FAILED_PRECONDITION with reason EXPECTED_COMMIT_MISMATCH,
  // SNAPSHOT_FAILED (metadata `workspace_id`), IMMUTABLE_REVISION or
  // HIDDEN_REVISION (metadata `commit_id`), CHANGE_DIVERGENT or
  // CHANGE_QUEUED (metadata `change_id`), INCLUDES_WORKING_COPY (metadata
  // `change_id`, `commit_id`, `workspace`), RANGE_TOO_LARGE (metadata
  // `count`), NO_DESTINATION, or a fetch's reasons (FetchGitRemote's).
  rpc RebaseChange(RebaseChangeRequest) returns (RebaseChangeResponse) {
    option (reef.api.v1.access) = ACCESS_WRITE;
  }

enum RebaseMode { REBASE_MODE_UNSPECIFIED = 0;  // branch
  REBASE_MODE_BRANCH = 1; REBASE_MODE_SOURCE = 2; }
message RebaseChangeRequest { string repo = 1; string workspace = 2;
  string revision = 3;                 // required; `@` needs `workspace`
  string onto = 4;                     // empty: trunk() after the fetch, or its remote bookmark when ahead
  RebaseMode mode = 5;
  string expected_commit_id = 6;
  bool skip_fetch = 7;
  bool skip_snapshot = 8; }
message RebasedChange { string change_id = 1; string old_commit_id = 2; string new_commit_id = 3;
  bool conflicted = 4; bool became_empty = 5; repeated string workspace_ids = 6; }
message RebaseChangeResponse {
  Change change = 1;                   // `revision`'s change afterwards
  Operation operation = 2;             // the rebase; unset when up to date
  Operation snapshot = 3;              // the named workspace's snapshot taken first
  Fetched fetched = 4;                 // unset when no fetch ran
  string onto = 5;                     // what the destination resolved from: `main`, `main@origin`, or the given revision
  string onto_commit_id = 6;
  repeated RebasedChange rebased = 7;  // newest first, at most 100
  uint32 rebased_count = 8;
  repeated Conflict conflicts = 9;     // introduced by this rebase, at most 10, files capped at 20, no hunks
  uint32 conflict_count = 10;
  uint32 inherited_conflict_count = 11;
  bool up_to_date = 12;
  bool started_new_change = 13;        // `revision` had landed: the workspace is on a new change on `onto`
  repeated string notes = 14; }        // sentences for a person: a local bookmark left behind, amended proposals, empty changes
```

`git.proto`:

```proto
// In GitService:
  // Fetches branches from a git remote configured in the repository and
  // records the remote-tracking bookmarks (and tracked local ones, except
  // git's checked-out branch) in one operation of kind FETCH. Runs the
  // daemon's git as PublishChange does. Never served through the remote
  // listener. Not undoable.
  //
  // Errors: INVALID_ARGUMENT with reason INVALID_BRANCH or
  // INVALID_GIT_REMOTE; NOT_FOUND with GIT_REMOTE_NOT_FOUND;
  // FAILED_PRECONDITION with NO_GIT_BACKEND, GIT_REMOTE_REQUIRED,
  // GIT_TOO_OLD; PERMISSION_DENIED with FETCH_DISABLED,
  // REMOTE_FETCH_DENIED or GIT_AUTH_FAILED; UNAVAILABLE with
  // GIT_UNREACHABLE, GIT_NOT_FOUND, GIT_FETCH_FAILED, FETCH_TIMEOUT or
  // FETCH_IN_PROGRESS.
  rpc FetchGitRemote(FetchGitRemoteRequest) returns (FetchGitRemoteResponse) {
    option (reef.api.v1.access) = ACCESS_WRITE;
  }

message FetchGitRemoteRequest { string repo = 1; string workspace = 2;
  string git_remote = 3;               // empty: trunk's remote, the only remote, else `origin`
  repeated string branches = 4; }      // empty: trunk()'s bookmark name, else the remote's HEAD branch
message FetchedBranch { string branch = 1; string commit_id = 2; string previous_commit_id = 3;
  bool local_moved = 4; bool missing = 5; }
message Fetched { string git_remote = 1; repeated FetchedBranch branches = 2;
  Operation operation = 3;             // unset when nothing changed
  repeated string notes = 4; }
message FetchGitRemoteResponse { Fetched fetched = 1; }
```

- `operations.proto`: `OPERATION_KIND_REBASE = 18`, `OPERATION_KIND_FETCH = 19`; `OperationFilter.kinds` takes both.
- `workspaces.proto`: `Workspace.behind_trunk = 23` (`optional uint32`): how many commits `::trunk() ~ ::<commit>` has, at most 1,000 (reads "1000" for more), as of the daemon's last ref import (a fetch, a colocated repository's import of a user's `git fetch`); set by `GetWorkspace` and `GetStatus` for a named workspace, unset elsewhere (a list would pay a walk per row), with no `trunk()`, and at an operation. `SnapshotWorkspaceResponse.started_on = 5`: the immutable commit the snapshot started a new change on, empty otherwise.
- **Reasons** (new): `REBASE_ONTO_DESCENDANT` (`INVALID_ARGUMENT`); `CHANGE_QUEUED` (`FAILED_PRECONDITION`); `GIT_FETCH_FAILED`, `FETCH_TIMEOUT`, `FETCH_IN_PROGRESS` (`UNAVAILABLE`); `FETCH_DISABLED`, `REMOTE_FETCH_DENIED` (`PERMISSION_DENIED`). The rest exist.
- **Remote rules**: `RebaseChange` `Rule::NoFetch` (new: a remote caller's request runs with `skip_fetch` set and a note "not fetched: the remote listener never fetches"; decision 9); `FetchGitRemote` `Rule::SocketOnly`.
- **ADR 0029's checklist** item by item in the PR; `API_REVISION` 30 with its digest; no paged method. Both SDKs regenerate.

### The CLI (decision 12)

```
reef rebase [REV] [--onto|-d|--destination REV] [-s|--source] [-b] [--no-fetch]
            [--expected-commit ID] [--no-snapshot]
reef fetch [--git-remote NAME] [--branch NAME]...
```

- `REV` defaults to `@`. `--onto` takes jj's spellings `-d` and `--destination` so an agent with jj habits succeeds first time. jj's `-o` is **not** taken: `-o` is Reef's global output flag (`-o json`). `reef rebase -o main` therefore fails as an unknown output format, and that error, when the command is `rebase` and the value is not a format, carries the hint "`-o` is the output format here; the destination is `--onto`" with `next` `reef rebase --onto main` (user decision 12, 2026-11-11); `--source` (`-s`) is source mode; `-b` is accepted as the branch mode's explicit flag. jj's `-r` (one commit, its children reparented) is refused with a hint (it is a history edit, not bringing work up to date).
- **Text**: `rebased kxyzwqrs onto main@origin (3 commits behind; 2 changes, 1 workspace moved)`, then each note, then for a conflict `conflict in kxyzwqrs: src/a.rs, src/b.rs` and `next: reef conflict show -r kxyzwqrs`; up to date: `kxyzwqrs is already on main's head`. A fetch line first when one ran: `fetched origin/main: 1a2b3c4 → 5d6e7f8`.
- **JSON**: `Rebased { change, operation, snapshot, fetched, onto, onto_commit_id, rebased, rebased_count, conflicts, conflict_count, inherited_conflict_count, up_to_date, started_new_change, notes, workspace, next }`. **Brief**: `change.id`, `onto`, `up_to_date`, `conflict_count`, `rebased_count`, `notes` when non-empty, `next` (`reef conflict show -r …`, `reef propose`, or null). `reef schema` gains `rebase` and `fetch`.
- **Exit codes** by kind: 0 rebased, up to date, or rebased with conflicts (the conflict is the answer, not a failure; `conflict_count` and `next` say so); 5 `FAILED_PRECONDITION`; 6 unavailable; 7 denied; 3 not found; 2 usage.
- `reef op log` prints `rebase change kxyzwqrs onto main@origin` and `fetch main from origin`; `--kind rebase|fetch` filters.
- `reef fetch` prints one line per branch (`origin/main 1a2b3c4 → 5d6e7f8`, `up to date`, `missing`).

### Discovery (decision 14)

- **`reef here`**: inside an active workspace with no divergence or attention, when `behind_trunk > 0`, the hint is "Its change is N commits behind main (as of the last fetch); `reef rebase` brings it onto main's head." and `next` is `reef rebase`. A workspace on trunk's head keeps today's hints. This is #262's "base is behind trunk" row only; the rest of #262's state machine stays there. `text_fits_twelve_lines` holds.
- **`reef status`** with a workspace prints `behind main by N` beside the parents when non-zero.
- **Root help**: `rebase` joins the "Also:" list after `describe`; `ui` leaves it (a person runs it; it stays in `--help-all`), at most 512 bytes, measured by the golden test. `fetch` is in `--help-all` only. `help_lists_every_command` gains `rebase`.
- **Agent guide** (long form; `AGENTS.md`'s short form is unchanged): the Changes paragraph's last sentence becomes "`reef rebase` brings a change onto the newest trunk; `reef publish -m <text>` pushes it to a git branch, and its `next` opens the pull request." About +50 bytes: the long form's budget rises from 1,700 to 1,750 and the file's from 1,750 to 1,800 (ADR 0071 amends ADR 0025's numbers).
- **`git rebase`/`jj rebase` habits**: `reef rebase` is now a real command, and its flags accept jj's spellings. `reef pull` and `reef sync` (and `reef merge`, whose hint says Reef rebases rather than merges) get a hint with `next` `reef rebase`, through a second small table beside `ORIENTATION_GUESSES` (`REBASE_GUESSES`), never an alias (ADR 0025). #261 folds that table into its own; this ticket adds only those rows.
- **Reef's own advice**: `reef land`'s conflict hint (`cli/land.rs:472`) becomes "`reef rebase --onto <bookmark>`, resolve, propose again, then land" with that `next`; `PublishChange`'s `BRANCH_MOVED` and `BRANCH_CONFLICTED` hints name `reef fetch --git-remote R --branch B` (with that `next`) instead of the jj and git commands; `BRANCH_DIVERGED`'s hint keeps `reef show` as `next` and adds "`reef rebase --onto R/B` to build on it".

### MCP, SDKs, web UI (decisions 15 and 16)

- **MCP**: a tool `rebase_change` ("Rebase change"; rpc `ChangeService/RebaseChange`; destructive, not idempotent) with `repo`, `workspace`, `revision` (default `@`), `onto`, `source` (bool) and `skip_fetch`; description "Bring a change, with the changes above it, onto the newest trunk (fetched first) or onto `onto`; workspaces on them get the new files. A conflict is returned as data with next. Returns {change, onto, up_to_date, conflicts, notes, next}." It returns `Rebased` (brief). `fetch_git_remote` stays out (`NOT_TOOLS`: "the rebase fetches; agents rarely need a fetch alone"). `list_operations` and `wait_for_operations` gain `rebase` and `fetch`. **The budget argument** (`tests/mcp.rs`'s comment asks for one): without the tool an MCP agent leaves Reef for `jj rebase` at the root, the workaround #259 records, and acts on the shared checkout instead of its workspace. The bound becomes the measured size rounded up to the next 50 bytes (about 23,300), and the comment records this ticket's reason.
- **SDKs**: generated. The hand-written workspace handles gain `rebase(**fields)` (Python sync and async) and `rebase(fields)` (TypeScript), which call `rebaseChange` with the handle's workspace and `revision` `@` and update the handle's change and commit from the response, as `move()` does; one test each.
- **Web UI**: `words.ts` `[OperationKind.REBASE]: "rebased a change"`, `[OperationKind.FETCH]: "fetched from a git remote"`; `log/counts.ts` counts both. The workspace panel shows "behind main by N" when `behind_trunk` is set (one line).

### Configuration

A `[fetch]` table in `config.toml`, strict, printed by `reef config show`:

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | `false` refuses every fetch (`FETCH_DISABLED`); `reef rebase` then needs `--no-fetch`, which the refusal's `next` gives |
| `timeout` | `"2m"` | how long a request waits for git, 10 s to 1 h |

`[publish] remotes` does not apply: fetching sends nothing of the repository's.

### Onboarding scenario G (decision 17)

| | Scenario | Setup | Harness | Prompt |
| --- | --- | --- | --- | --- |
| G | Catch up | F's setup (bare `origin`); a workspace created on `main` with an edit to `README.md`, snapshotted; then a commit to `src/lib.rs` pushed to the bare remote's `main` from a second clone, not fetched | as B, starting inside the workspace (`cd` to its path) | `g.txt`: "main has moved on the remote since this change was started. Bring this change up to date with the remote's main, keeping your README edit. Print the change id on your last line." |

The judge reads the repository: the change named on the last line has one visible commit whose parent is the bare remote's `main` head, and its tree has both the README edit and the remote's `src/lib.rs`. "Calls to success" counts up to the first `reef rebase` that exited 0; "Escaped" counts `jj rebase`, `jj git fetch`, `git fetch`, `git pull`, `git rebase`. Held to B's targets; the first CI dispatch starts G's baseline.

## Deliverables

### `crates/reef-api/`

- `changes.proto` (`RebaseChange`, `RebaseMode`, `RebasedChange`, the messages), `git.proto` (`FetchGitRemote`, `Fetched`, `FetchedBranch`), `operations.proto` (two kinds), `workspaces.proto` (`behind_trunk`, `started_on`), with comments and "Errors:" paragraphs; `src/lib.rs`: the reasons, `API_REVISION` 30 and its digest.

### `crates/reef-lib/`

- `src/jj/fetch.rs` (new): `FetchPlan`, `Repo::plan_fetch`, `fetch(repo: Arc<ReadonlyRepo>, plan, options) -> Result<FetchedRefs, FetchError>` (throwaway transaction), `Repo::record_fetch(&FetchedRefs, &OpMeta) -> RecordedFetch`; `FetchError` implements `Classify`.
- `src/jj/push.rs`: `push_environment` → `git_environment` (shared), `classify_stderr` and `redact` used by both; no behavior change for publish.
- `src/jj/rebase.rs` (new): `RebaseRequest`, `Repo::plan_rebase` (set, checks, destination resolution including the remote counterpart, the landed-`@` case), `Repo::rebase(&plan, &OpMeta) -> Rebased` (`move_commits`, `became_empty`, introduced and inherited conflicts by `conflict::read`), and `behind_trunk(repo, commit) -> Option<u32>`.
- `src/jj/rewrite.rs`: `start_on(tx, names, commit)`, used by `land.rs`, `rebase.rs` and the follower; `land.rs` calls it.
- `src/jj/snapshot.rs`: the immutable arm (a new child instead of a rewrite of a commit in `::trunk()` or under a tag); `PendingSnapshot::started_on`.
- `src/jj/oplog.rs`: `OpKind::Rebase` (`"rebase"`), `OpKind::Fetch` (`"fetch"`), and `classify` for both descriptions.

### `crates/reef-daemon/`

- `src/workspace/rewrite.rs`: `Write::Rebase { request }`, `Write::StartOnLanded { commits, transitions }` (no step-3 snapshot), the new-child carry in `carries()`.
- `src/fetch.rs` (new): the per-(repository, remote) lock, the timeout, `[fetch]`, the advice; used by `service/git.rs` and `service/changes.rs`.
- `src/service/changes.rs` (`rebase_change`), `src/service/git.rs` (`fetch_git_remote`), `convert.rs`, `op_filter.rs`, `error.rs`, the undo planner (fetch refused), `remote/rules.rs` (`Rule::NoFetch`, two rows), `service/workspaces.rs` and `service/status.rs` (`behind_trunk`).
- `src/lifecycle/observe.rs` and `lifecycle/mod.rs`: the follower over active workspaces; `record_observed` through `Write::StartOnLanded` when a workspace sits on the landed commit.
- `src/workspace/snapshot.rs`: the row's change after an immutable-arm snapshot; the description.
- `src/config.rs`: `[fetch]`; `config/tests.rs`.
- `README.md`: the rebase and fetch operations' descriptions.

### `crates/reef-client/`, `crates/reef-vcs/`

- `Client::rebase_change`, `Client::fetch_git_remote`; JSON and brief `Rebased`, `Fetched`.
- `src/cli/rebase.rs` (new, both commands); `cli/mod.rs` (the commands, `REBASE_GUESSES`); `cli/help.txt`; `cli/here.rs`; `cli/status.rs`; `cli/op.rs`; `cli/land.rs` (advice); `cli/schema/outputs.rs`; `mcp/tools.rs`, `mcp/args.rs`; `docs/agent-guide.md` and `cli/guide.rs` (the budget); the publish advice in `reef-daemon/src/publish/mod.rs`.

### `sdk/`, `web/`, `crates/reef-testing/`

- `sdk/python/src/reef/workspace.py`, `aio/workspace.py`, `sdk/typescript/src/workspace.ts`: `rebase`, with tests.
- `web/src/ui/words.ts`, `web/src/log/counts.ts`, the workspace panel line, with tests; the generated code.
- `src/onboarding/`: `Scenario::G`, setup, judge, escape list, `prompts/g.txt`, a judge fixture.

### Documentation

- **`docs/rebasing.md`** (new): what `reef rebase` does (fetch, destination, branch and source modes, descendants and other workspaces, what the files on disk become and what it costs); every refusal and what to do; conflicts (markers on disk, `reef conflict show`, the assignment); empty changes after a squash merge; proposals (amended, propose again; queued refused); a landed `@`; undo (the rebase yes, the fetch no); `reef fetch`, credentials and failures (pointing at `docs/publishing.md`'s section), a local bookmark left behind; workspaces on a landed commit and the snapshot backstop (#248).
- **`docs/adr/0071-rebasing-and-following-trunk.md`** (accepted), indexed:
  - **Decisions**: the rebase is a `Manager::rewrite` write in jj's branch mode by default, one operation, undoable; fetch first by default, through jj-lib's `GitFetch` on a throwaway transaction off the repository thread and a replayed record on it, never moving git's checked-out branch, not undoable; the destination is `trunk()` or its remote bookmark when ahead; conflicts are written and returned; no lifecycle transition, `QUEUED` refused; another live workspace below `REV` refused; a landed `@` starts a new change; workspaces whose commit reaches `trunk()` get a new child in the landing's operation; the new-child carry (amends ADR 0039); the snapshot's immutable arm (amends ADR 0018); `Rule::NoFetch`; the guide budget (amends ADR 0025's numbers).
  - **Alternatives**: `jj rebase -s @` as the default; merging (a merge commit) instead; fetch and rebase in one operation; the fetch transaction committed off the repository thread (a concurrent operation the thread merges); fetching on the repository thread; moving the local trunk bookmark on fetch; a failed fetch as a note; rebasing another workspace's change silently; auto re-proposing; `EmptyBehavior::AbandonNewlyEmpty`; the follower alone; the backstop alone; refusing the snapshot instead of starting a change; an `ATTENTION_STALE` mark (#202); `move_workspace` with a rebase flag instead of a tool.
- **`docs/threat-model.md`**: the "Publishing (#258)" asset row becomes "Git remotes (#258, #259)" and adds fetching. The remote listener requirements gain "`FetchGitRemote` is never served remotely, and `RebaseChange` never fetches there". Rows:
  - **S-177**, Rebasing another agent's live work, an agent, Medium: a branch-mode rebase moves an ancestor change another workspace is still writing. Fixed: `INCLUDES_WORKING_COPY` below `REV`; descendants move with carries (ADR 0039). Tests: `rebase::refusals`, `rebase::stack`.
  - **S-178**, Post-landing edits moving the destination through an observed landing (#248), the agent whose workspace sat on the commit, High. Fixed: the follower gives such workspaces a new child in the landing's operation; the new-child carry; the snapshot never rewrites a commit on `trunk()`. Closes S-121's remaining hole. Tests: `follow::observed_landing`, `follow::unrecorded`, `snapshot::immutable_arm`.
  - **S-179**, A reviewed change replaced by a rebased tree, an agent, Medium: a proposal rebased after review lands a tree nobody reviewed. Fixed: no transition, `amended: true`, `PROPOSAL_AMENDED` at landing until proposed again; `QUEUED` refused. Tests: `rebase::lifecycle`.
  - **S-180**, The user's git credentials through the remote listener by fetching, a `write` token, Medium. Fixed: `FetchGitRemote` socket only; `RebaseChange` never fetches remotely. Tests: `fetch::remote_listener`.
  - **S-181**, Data leaving through fetch refspecs, a sandboxed same-uid agent, Low: branch names an agent chooses reach the remote server. Partly fixed: `reef rebase` fetches only trunk's name; `reef fetch --branch` names are checked as #258's; `[fetch] enabled`; the Harness sandboxes paragraph names it.
  - **S-182**, A fetch that prompts or hangs, Medium: as S-172, with `[fetch] timeout`, `FETCH_IN_PROGRESS`, and the fetch never holding the repository thread or the create lock. Tests: `fetch::timeout`.
  - **S-183**, A person's checkout moved by a fetch, an agent, Medium: a fetch fast-forwards a tracked local bookmark that is git's checked-out branch, and the jj CLI's next export moves the person's branch. Fixed: never merged; the remote bookmark moves and the rebase targets it. Tests: `jj_fetch::checked_out`.
  - **S-184**, Disk growth from rebasing many workspaces over a large trunk jump, any writer, Low. Documented: each moved workspace rewrites the files that differ between its old and new trees, reported by `DiskUsage`; `RANGE_TOO_LARGE` caps the set; re-basing a workspace's base is a follow-up.
- **`docs/quickstart.md`**: "Bring your change up to date" after "Look at what changed": `reef rebase` from inside the workspace, what a conflict looks like, `reef here`'s hint. **`docs/onboarding-benchmark.md`**: scenario G. **`docs/landing.md`**: the conflict paragraph says `reef rebase --onto main`; "workspaces on the landed commit" covers observed landings and the backstop. **`docs/proposals.md`**: rebasing a proposal; observed landings move workspaces. **`docs/conflicts.md`**: conflicts from `reef rebase` and their assignment. **`docs/publishing.md`**: `BRANCH_MOVED`'s fetch is `reef fetch`; the out-of-scope line for #259 removed; "after a squash merge, `reef rebase` shows the change empty". **`docs/design.md`**: the git interop paragraph links `docs/rebasing.md`. **`docs/agent-guide.md`**, as above.
- **`README.md`** "Phase 3 so far": a bullet for rebasing and fetching, API revision 30. **`CHANGELOG.md`** Unreleased: Added (`RebaseChange`, `FetchGitRemote`, `reef rebase`, `reef fetch`, `rebase_change`, SDK `rebase`, `behind_trunk`, `[fetch]`, scenario G); Changed (root help, guide, `reef here`, `land` and `publish` advice); Fixed (#248).
- **`docs/dev-phases.md`** R4.14's row and this directory's README "Departures" bullet: see "Departures" below; the README table row is added with this brief.

## Tests

Daemon and lib tests that need git refs use a **colocated** repository (as #258 found), and every git command in a test that creates commits sets `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL` (and dates where an id must be stable). Fetch tests use a bare remote through `file://` and print a skip note and return when `git` is not on `PATH`. New fixtures use `testing::no_automatic_snapshots`.

In `crates/reef-lib/` (`tests/jj_fetch.rs`, `tests/jj_rebase.rs`):

1. `fetch_plan`: the remote of a remote `trunk()`, the only remote, `origin` among several, `GIT_REMOTE_REQUIRED`; the default branch from `trunk()` and from the remote's HEAD; names refused as #258's test 7.
2. `fetch_and_record`: a commit pushed to the bare remote from a second clone is fetched; the view's `main@origin` moves; a tracked local `main` fast-forwards; an untracked one stays; nothing else in the view changes; the operation's kind and description; a second fetch changes nothing and writes no operation.
3. `record_matches_jj`: for the same fetched state, the replayed record and jj-lib's own `GitFetch::import_refs` committed in a scratch copy give equal views (remote bookmarks, local bookmarks, heads), including a remote branch force-pushed backwards and a deleted one.
4. `checked_out` (S-183): a colocated repository with `HEAD` on `refs/heads/main` and `main` tracking `main@origin`: the fetch moves `main@origin` only, with the note.
5. `rebase_plan`: branch mode over a stack of two (both roots and descendants), source mode, `REBASE_ONTO_DESCENDANT`, `IMMUTABLE_REVISION`, `CHANGE_DIVERGENT`, `RANGE_TOO_LARGE` (cap lowered through a test hook), up to date (parents already `D`; `REV` in `::D`).
6. `destination`: `trunk()` local and current; local `main` behind `main@origin` chooses `main@origin` with the note; diverged (neither descends) chooses `trunk()`; `onto` given wins; no trunk and no `onto` is `NO_DESTINATION`.
7. `rebase_conflicts`: two changes editing one line: the rebase is written, the commit has a conflict, `conflicts` lists one introduced conflict with its files, a descendant only inheriting it is counted in `inherited_conflict_count`.
8. `became_empty`: a change whose diff trunk already has is kept, reported empty.
9. `start_on`: the helper gives named jj workspaces a new empty child; `land_chain` still passes R3.13's tests through it.
10. `immutable_arm` (unit, `snapshot.rs`): a fresh working copy on a commit in `::trunk()` with an edit plans a new child with the scanned tree; one under a tag likewise; a mutable commit is rewritten as before; the divergent and recover arms on an immutable commit plan a child too.

In `crates/reef-daemon/` (`tests/rebase.rs`, `tests/fetch.rs`, `tests/follow_trunk.rs`, through `TestDaemon`):

11. `from_a_workspace`: a workspace on old `main` with an edit; `main` advanced in the bare remote; `RebaseChange` with `revision` `@` fetches, rebases, and the workspace's directory has trunk's new file and keeps its edit; `snapshot`, `fetched`, `operation` in the response; `ListOperations` shows `FETCH` then `REBASE`; `CheckoutStats` of the follow counts only the files trunk changed (the resource check).
12. `late_edit_carried`: an edit written after step 3 (the `follow_hold_point` hook) ends in the rebased change, not divergent (ADR 0039 through the rebase).
13. `stack`: workspace B on workspace A's change; `reef rebase` from A moves both, B's directory updated; from B, `INCLUDES_WORKING_COPY` naming A with `next` `reef rebase --workspace <A>`; `--source` from B moves only B.
14. `refusals`: `CHANGE_QUEUED` (a queued change through R3.13's queue), divergent, immutable explicit `REV`, each with hint and `next`.
15. `landed_at`: the workspace's commit made `main` by hand: `reef rebase` starts a new empty change on `main`'s head, `started_new_change`, one operation.
16. `lifecycle`: a proposed change rebased: still `PROPOSED`, `amended: true`, `next` `reef propose`; `LandChange` refuses with `PROPOSAL_AMENDED` until proposed again.
17. `conflict`: the conflicting case returns a `Conflict` object, `next` `reef conflict show -r <change>`, the workspace's files carry jj's markers, an assignment is open and owned by the workspace's session (R3.27).
18. `up_to_date_and_undo`: a second `reef rebase` records nothing (no operation, `up_to_date`), exit 0; `UndoOperation` of the rebase returns the change and the workspace's files; undo of the fetch is `OPERATION_NOT_UNDOABLE`.
19. `fetch`: `FetchGitRemote` failures through a fake git (auth, unreachable, timeout with `[fetch] timeout = "10s"` then `FETCH_IN_PROGRESS`); a failed fetch fails `RebaseChange` with `next` `… --no-fetch`; `[fetch] enabled = false`.
20. `remote_listener`: a `write` token's `RebaseChange` rebases without fetching (note); `FetchGitRemote` is `REMOTE_FETCH_DENIED`.
21. `follow_trunk` (#248): (a) a proposed change on a workspace's `@` reaches `main` by an external jj bookmark move: one `TRANSITION` operation records `LANDED` **and** the workspace's new empty working-copy commit; a write in the workspace then snapshots into the new change and `main` does not move; (b) the same with an edit on disk at the move: the edit lands in the new change (the new-child carry), never in the landed commit; (c) an unrecorded change reaching `main`: a `move-workspace` operation per workspace; (d) the backstop: with the follower paused through a test hook, an edit and an explicit snapshot start a new change on the landed commit, `started_on` is set, `main` does not move; (e) R3.13's land with an edit between step 3 and the write: no divergence. The backstop's check is timed over 1,000 snapshots on `medium` against a build without it: within noise, and under 50 µs per check.
22. `behind_trunk`: `GetWorkspace` reports 0 on trunk's head, N after N commits land, unset without `trunk()`; capped at 1,000.

In `crates/reef-vcs/` (`tests/cli.rs`, `tests/schema.rs`, `tests/mcp.rs`):

23. `rebase_cli`: `reef rebase` from inside a workspace in text, `-o json`, `-o brief` and piped, validated against `reef schema`; `-d`, `--destination`, `--onto`, `-s`, `-b`, `--no-fetch`; `-o main` refused with the `--onto` hint and `next`; jj's `-r` refused with a hint; exit 0 with a conflict; `reef fetch`; `reef here`'s behind hint and `next`; `reef pull`, `reef sync` and `reef merge` give a hint with `next` `reef rebase`, exit 2; `reef op log --kind rebase`; the root help golden file and its budget; the guide's budgets at 1,750 and 1,800; `rebase_change` in `tools/list` under the new bound, its result validated against its output schema; the kinds in both enums; `reef land`'s conflict `next`.

Elsewhere: scenario G's judge on fixtures and `--offline` setup (`crates/reef-testing/`); the SDK handle tests (Python, TypeScript); vitest for the words, counts and panel line; the digest at revision 30, `just api` and `just api-breaking` (`crates/reef-api/`).

## Implementation notes

- **Order**: `start_on` and the new-child carry (tests 9, 21e); the snapshot backstop (10, 21d); the follower and `record_observed` through `Write::StartOnLanded` (21a–c); the rebase plan and write without fetch (5–8, 11–18); the fetch plan, throwaway fetch and record (1–4); the daemon fetch module and failures (19, 20); `behind_trunk` (22); the API, CLI, schema, MCP, help, guide and here (23); SDKs, web, scenario G, documents. #248's part is useful alone and lands first in the PR's history.
- **Threads and locks**: as #258. The fetch's network call holds only the fetch lock; the plan and record are repository-thread jobs under the create lock shared. The rebase is one `Manager::rewrite`, which takes the create lock itself; the fetch's record finishes and drops its guard before the rewrite starts (taking it twice would wait behind GC's queued writer). The follower's `record_observed` already holds the create lock shared and the lifecycle lock, so it uses `rewrite_held`.
- **The throwaway transaction** is started from the plan's `Arc<ReadonlyRepo>` inside the blocking closure and dropped, never committed, as #258's push.
- **Verify** jj-cli 0.47.1's `rebase -b` set (`roots(D..REV)::`) and its handling of an immutable working-copy commit against Reef's, and say in the PR where Reef differs. Reef does not depend on jj-cli.
- **Dogfood**: start this ticket's own PR from a workspace behind `main` and bring it up to date with the new `reef rebase` (replacing the runbook's `jj git fetch` and `jj rebase` at the root); log the friction in the PR.
- **Linux**: nothing is `cfg(target_os = "linux")`, but the overlay backend's follow (copy-up of the changed files) runs in the privileged container: `tests/rebase.rs` and `tests/follow_trunk.rs` there before pushing.

## Performance expectations, and the resource criterion

A rebase updates a workspace's directory by jj's tree-diff check-out: **only files that differ between the workspace's old and new trees are written or removed**; no re-materialization, no new base cache entry, no copy of the tree. Disk growth per moved workspace is the size of those files (they no longer share blocks with the base on the clone backends; on overlay they are copied up), which `DiskUsage` reports. CPU per moved workspace is the step-3 snapshot (a narrowed scan when the watcher has one), one stale-arm full scan in the follow, and the check-out of the changed files. The fetch is one branch's new objects.

Release build, M2 Max, local bare remote:

| Measurement | What | Expect |
| --- | --- | --- |
| `RebaseChange`, no fetch, `large` (100k files), trunk +100 files changed | one workspace | < 500 ms, of which the follow's scan about 150 ms |
| The same on `medium` (10k files) | one workspace | < 150 ms |
| Files written by the follow | trunk +100 files | exactly the files differing between the trees (`CheckoutStats`), asserted in test 11 |
| Disk growth, APFS clone | trunk +100 files of 4 KiB | ≤ their bytes + 10%, `DiskUsage` before and after |
| `FetchGitRemote`, one branch, 10 new commits | local bare remote | < 300 ms, most of it git's process |
| Snapshot backstop check | per snapshot | < 50 µs |
| `behind_trunk` | 1,000-commit cap | < 5 ms |

The first three are an acceptance criterion (#264's budget: five tickets on a standard MacBook): measured by a manual test `rebase_resource` in `tests/rebase.rs` (`--ignored`, the `large` preset) with its numbers in the PR and in `docs/benchmarks.md`.

## Acceptance criteria

- [ ] An agent inside a workspace whose base is behind the remote's `main` runs `reef rebase` and ends on `main`'s head, files on disk updated and its own edits kept (tests 11, 12; scenario G).
- [ ] A conflicting rebase succeeds and returns a conflict object with `next` `reef conflict show` (tests 7, 17).
- [ ] Already up to date records nothing and exits 0; the rebase is one undoable operation (test 18).
- [ ] Other workspaces on moved changes follow with their files updated; another agent's live change below `REV` is refused with `next` (test 13).
- [ ] Proposed changes read `amended` and need `reef propose`; queued changes are refused (tests 14, 16).
- [ ] #248: a workspace on a commit that reaches `trunk()` gets a new change in the landing's operation; no snapshot ever moves `main`; late edits land in the new change (test 21).
- [ ] `reef here` suggests `reef rebase` when behind; root help, guide, `pull`/`sync`/`merge` hints, `land` and `publish` advice (tests 22, 23).
- [ ] API revision 30; `RebaseChange`, `FetchGitRemote`, `rebase_change`, SDK `rebase`.
- [ ] Resource criterion: only differing files are written, disk growth bounded as stated, timings in the PR (test 11, `rebase_resource`).
- [ ] ADR 0071, `docs/rebasing.md`, the threat model and other documents; `just ci` and `just doc` pass; the Linux container's build, clippy and the two test files pass.

## Verification

```sh
export CARGO_TARGET_DIR=/Users/arsood/Projects/ReefVCS/target-259 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only
cargo nextest run -p reef-lib --test jj_fetch --test jj_rebase --test jj_land
cargo nextest run -p reef-daemon --test rebase --test fetch --test follow_trunk --test landing --test lifecycle --test api_v1
cargo nextest run -p reef-vcs --test cli --test schema --test mcp
cargo nextest run -p reef-testing onboarding
cargo nextest run -p reef-api
(cd web && npm test); (cd sdk/python && uv run pytest); (cd sdk/typescript && npm test)
cargo test --release -p reef-daemon --test rebase -- --ignored rebase_resource --nocapture
# By hand, in a scratch repository with a bare remote:
git init --bare /tmp/remote.git && git remote add origin /tmp/remote.git && git push origin main
WS=$(reef ws create -o json | jq -r .id); echo note >> "$(reef ws inspect $WS -o json | jq -r .path)/README.md"; reef ws snapshot $WS
# push a commit to /tmp/remote.git's main from a second clone (GIT_AUTHOR_*/GIT_COMMITTER_* set), then:
reef here --workspace $WS; reef rebase --workspace $WS; reef here --workspace $WS; reef undo <the rebase op>; reef ws release $WS
just doc && just ci
```

## Out of scope

- **#202 and #91**: a descendant left stale, or diverged when busy, by an ancestor's **snapshot** (decision 10).
- **A rebase preview** (`dry_run`), and **`reef merge`** (a merge commit instead of a rebase).
- **Re-basing a workspace's base cache entry** after a large trunk jump, so its directory shares blocks with a newer base again (decision 13).
- **The rest of #262** (`reef here`'s full lifecycle state machine) and **#261** (the full git-verb table).
- **Fetching more than named branches** (all branches, tags, prune), **forwarding the client's SSH agent**, **native gitoxide fetch** (R4.14).
- **#92** (a conflicted check-out whose working-copy state is not saved reads its markers back as text): a conflicting rebase checks out markers through the same path; the risk is #92's.

## Risks

- **The fetch's record replays jj-lib's private import merge** with public calls. Mitigated by test 3, which compares it with jj-lib's own import, and by the narrow scope (named branches, no tags, no abandoning of unreachable commits); a jj-lib upgrade that changes the import fails test 3.
- **Branch mode moves more than the agent's own change** when it is stacked. Mitigated: another live workspace below `REV` is refused; descendants follow with carries; `--source` is one flag away.
- **The stale-arm full scan** in each follow costs O(tree) per moved workspace. Accepted and measured; a narrowed follow is a follow-up if `rebase_resource` exceeds its budget.
- **A failed fetch fails the rebase** (decision 8), costing an offline agent one retry with the `next` given.
- **Three budgets rise** (`tools/list` by about 900 bytes, the guide by 50, the root help by a word swap). Decisions 14 and 15 give the alternatives.
- **Field and kind numbers** collide with another ticket's at rebase; whoever merges second renumbers.

## Choices this brief made, and why

- **Branch mode by default**, because `git rebase main` and `jj rebase -o main` both move the whole branch, and moving only `@` off a parent that has not landed drops the parent's edits from the agent's tree.
- **The remote bookmark as the destination when it is ahead**, because a git user's local `main` is rarely fast-forwarded, and Reef must not move it to get there.
- **Fetch, then rebase, as two operations**, so undo of the rebase is exact and never disagrees with git's fetched refs.
- **Conflicts written, not refused**, because the issue asks that a rebase never leave the agent with nothing to do, and R3.25/R3.27 give it the conflict and its ownership.
- **No transition**, because R3.09 already reads a rebased proposal as amended, and re-proposing is the agent's statement that the new tree is ready.
- **The follower and the backstop**, because the follower records the new change in the landing's operation (the issue's expectation) and the backstop closes the window the follower cannot (a snapshot before it acts, a follower failure, a tag).

## What later tickets inherit

- **#202/#91** reuse `follow` and the carries from the snapshot path.
- **R3.10/R3.11** rebase stacks and dependents through `Write::Rebase` (source mode, a set of roots), with conflicts as data.
- **#262** builds its state machine on `behind_trunk`; **#261** absorbs `REBASE_GUESSES`.
- **R3.17** fetches the destination before pushing landed history through `reef_lib::jj::fetch`; **R4.14** replaces the subprocess under both fetch and push.
- **R3.26** (agent-scoped undo) undoes rebases like any rewrite.

## Departures from dev-phases.md

These issues have no row. To be made in the implementing PR (this brief adds only the README table row):

- **R4.14 hardens the fetch too**: #259 delivers the first fetch (`reef fetch` and `reef rebase`'s, through jj-lib's git subprocess with the user's git credentials, on the push's environment), and R4.14 makes it native. The R4.14 row and a README "Departures" bullet say so.
- **R3.13's S-121 is closed for observed landings** here; R3.13's brief keeps its decision 10 as history.

## Decisions for the user

Decided 2026-11-11: the user accepted every recommendation, with decision 12 changed (no `-o`). Planned numbers: API revision 30, ADR 0071, threat rows S-177 to S-184, operation kinds 18 (`rebase`) and 19 (`fetch`), no migration (schema stays 17).

**Group 1: scope**

1. **Size and split.** L (about 3,000 lines), #248 in the same ticket. *Recommended*: as stated. Alternatives: M at its top end by dropping `FetchGitRemote`/`reef fetch` (the fetch stays inside the rebase), scenario G and the SDK handle methods to follow-ups; or #248 as its own S ticket first (it is useful alone, but the new-child carry and `StartOnLanded` are then built before the rebase that also uses them).

**Group 2: what the rebase does**

2. **The set.** Branch mode by default (`jj rebase -b`, `git rebase main`): every commit of `REV`'s ancestry the destination lacks, with descendants; `--source` for `REV` and its descendants only; another live workspace's change below `REV` refused with `next` `reef rebase --workspace <id>`. *Recommended*: as stated. Alternatives: source mode by default (drops an unlanded parent's edits from the tree); branch mode that moves other workspaces' ancestor changes silently (as jj does).
3. **The destination.** After the fetch, `trunk()`, or its remote bookmark (`main@origin`) when that is strictly ahead of a local `trunk()`; the local bookmark is never moved to get there. *Recommended*: as stated. Alternatives: `trunk()` only (a git user's stale local `main` makes `reef rebase` a no-op after a fetch); fast-forward the local bookmark on fetch (moves a person's branch; S-183).
4. **A landed `@`.** `reef rebase` on a workspace whose change has landed starts a new empty change on the destination (`started_new_change`). *Recommended*: as stated. Alternative: `IMMUTABLE_REVISION` with `next` `reef ws move <id> --new-on main`.
5. **Conflicts.** Written, exit 0, R3.25 objects for introduced conflicts (at most 10, 20 files each, no hunks), `next` `reef conflict show -r <change>`, R3.27's assignment opens. *Recommended*: as stated (the issue's acceptance). Alternative: exit 5 with the conflict in the error's metadata (scripts notice, but it reads as a failure the agent must undo).
6. **Lifecycle.** No transition; a rebased `PROPOSED` change reads `amended` and `next` is `reef propose`; `QUEUED` refused (`CHANGE_QUEUED`). *Recommended*: as stated. Alternatives: re-propose automatically after a clean rebase (reviewers' view silently replaced; S-179); allow rebasing a queued change and let R3.13 dequeue it `AMENDED`.

**Group 3: fetching**

7. **Mechanism.** `GitFetch` on a throwaway transaction off the repository thread, then the import's merge replayed with public jj-lib calls on the repository thread; fetch and rebase are two operations, the fetch not undoable; `reef fetch` and `GitService.FetchGitRemote` added (help-all only, not an MCP tool). *Recommended*: as stated. Alternatives: commit the fetch's own transaction off the repository thread (simpler, but amends ADR 0012 and lets a concurrent operation appear that the thread must merge); fetch on the repository thread (blocks every snapshot of the repository for the network call); fetch and rebase in one operation; no `reef fetch` (publish's `BRANCH_MOVED` keeps its jj/git command).
8. **A failed fetch.** Fails the rebase with `next` `reef rebase … --no-fetch`. *Recommended*: as stated (an agent never believes it is on the newest trunk when it is not). Alternative: rebase onto the last-fetched trunk with a note (one call offline, but easy to miss).
9. **Remote listener.** `RebaseChange` served to `write` tokens without fetching (a note says so); `FetchGitRemote` socket only. *Recommended*: as stated. Alternatives: `RebaseChange` socket only; refuse a remote rebase unless `skip_fetch` is set explicitly.

**Group 4: #248**

10. **Following trunk.** A follower gives every active workspace whose commit reached `trunk()` a new empty change (in the observed landing's own operation for a recorded change); a new-child carry keeps late edits out of the landed commit; a snapshot never rewrites a commit on `trunk()` or under a tag and starts a new change instead; #202 (an ancestor's snapshot leaving a descendant stale) stays out with #91. *Recommended*: as stated. Alternatives: backstop only (S; the landing's operation does not move the workspace, which the issue expects); follower only (leaves the settle window and tags open); #202 in scope as an `ATTENTION_STALE` mark (about 150 lines, no fix).

**Group 5: the surface**

11. **API.** `ChangeService.RebaseChange` (WRITE) and `GitService.FetchGitRemote` (WRITE, socket only); `Workspace.behind_trunk`; `SnapshotWorkspaceResponse.started_on`; revision 30. *Recommended*: as stated. Alternatives: `RebaseChange` on `WorkspaceService` (but it rebases changes, with or without a workspace); a `dry_run` preview now.
12. **CLI.** `reef rebase [REV] [--onto|-d|--destination] [--source|-s] [-b] [--no-fetch]`, jj's `-r` refused with a hint; exit 0 on a conflict. *Decided*: as stated, **without `-o`** (Reef's global output flag); `-o <not a format>` on `rebase` gets the `--onto` hint and `next`. The brief first proposed `-o` too. Alternatives: no jj spellings (`--onto` only); `reef sync` as the command name (the issue allows it; ADR 0025 makes guesses hints, never aliases).
13. **Resource criterion.** Only files that differ between a workspace's old and new trees are written; disk growth bounded by their bytes + 10%; `large` + 100 changed files < 500 ms; measured by `rebase_resource`. Re-basing the workspace's base cache entry is a follow-up. *Recommended*: as stated. Alternative: move the workspace to a fresh base on large jumps (the move path's `swap_base`) now, which shares blocks again but costs a base materialization.
14. **Discovery and budgets.** `reef here` suggests `reef rebase` when `behind_trunk > 0`; the root help's "Also:" gains `rebase` and drops `ui`; the guide's Changes sentence names `reef rebase`, its budgets rising to 1,750 (long) and 1,800 (file); `pull`/`sync`/`merge` get hints; `reef land`'s and `reef publish`'s advice point at `reef rebase`/`reef fetch`. *Recommended*: as stated. Alternatives: keep the guide's budgets by trimming another line; drop another word than `ui` (`schema`, `presence`); leave `pull`/`sync` to #261.
15. **MCP.** A `rebase_change` tool, `tools/list`'s bound raised to the measured size rounded up to 50 (about 23,300), argued in the test's comment. *Recommended*: as stated. Alternatives: a `rebase` flag on `move_workspace` (about 300 bytes instead of about 900, but a different shape from the RPC, which the issue asks to match); `NOT_TOOLS`.
16. **SDKs.** `rebase()` on the Python and TypeScript workspace handles, updating the handle. *Recommended*: as stated. Alternative: generated clients only (a follow-up issue).
17. **Onboarding.** Scenario G ("catch up"): a workspace behind the bare remote's `main`, judged from the change's parent and tree. *Recommended*: as stated. Alternatives: no scenario (the daemon and CLI tests cover the acceptance); a rebase step added to F (F's baseline restarts).
