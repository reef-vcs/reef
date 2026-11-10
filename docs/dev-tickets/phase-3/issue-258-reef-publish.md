# Issue #258 `reef publish`: push a change to a git remote branch

Phase 3 · Size M, at its top end, with four follow-ups (decision 1) · Fixes: issue #258 (question 2 of #194, the publish path) · Depends on: R3.09 (snapshot-first for `@`, the `CHANGE_EMPTY`, `CHANGE_CONFLICTED` and `CHANGE_DIVERGENT` checks), R1.16 (the `snapshots` index and its `local` flag), and builds on R1.05 (git ref import), R2.02 (revisions, `trunk()`), R2.03 (`OpMeta`, undo's refusals), R2.08 (remote rules), R2.13 (`reef mcp` and its budget), R2.27 (the CLI's writes and advice), ADR 0051 (task-first help) and RX.07 (the onboarding benchmark) · Unblocks: R3.17 (pushes landed history through this ticket's push path); through it R4.14

## Context

Reef has no command that takes work to a git remote. Every one of Reef's own ticket PRs left Reef to publish: `jj describe`, `jj bookmark set` and `jj git push` from the root checkout, then `gh pr create`. `reef push` and `reef pr` are unrecognized. R3.17 pushes only after a Reef landing, so a team whose work reaches `main` as pull requests has no path. #194 found this was the main reason Reef did not fit a large monorepo on GitHub; its comment gives question 2 (a publish path through Reef) to this issue and leaves git inside workspaces (questions 1 and 3) there.

The design asks for this in three places. Workspaces: "These snapshots stay on the machine. Only a change that an agent or human explicitly publishes leaves it, so half-written edits never reach a shared store." Durability: "Local-only work is the exposed state on a laptop … The mitigations are … publishing early." Federation: "Git interop is the adoption path for years. Humans review on GitHub and will for the foreseeable future … speaking the git wire protocol natively rather than shelling out" (R4.14 does that; this ticket uses jj-lib's git support as it stands).

The issue's proposal is the shape: `reef publish [REV] [--branch NAME]`, snapshotting `@` first as `reef propose` does; refusing an empty, conflicted, divergent or undescribed change with `hint` and `next`; pushing through the daemon (the CLI keeps no jj-lib) to a branch named from the change id, so publishing again updates it; printing the branch, the remote, the commit, and in `next` the command that opens a pull request on a known host. Reef opens no pull request itself: it holds no host credentials. The issue's comment adds three scope notes from Reef's dogfooding runbook: publishing again must update, not create; `reef describe` must carry trailers (`Signed-off-by:`, co-author lines) unchanged, with a sign-off option considered; and jj skips git's hooks, so the brief must say whether publishing can require declared checks first.

Earlier tickets left seams here:

- **R1.16** (`snapshot/index.rs` module docs): "R4's publish clears `local` on the rows of the change it publishes ([`for_change`]), and an export refuses the rest … R4's publish, which clears `local`, has to survive [GC's reindex]: carry the flag by commit id, or record published changes elsewhere." This ticket is that publish, to a git remote rather than R4's store.
- **R3.09**: `reef propose` resolves the revision with `uses_working_copy` and snapshots it with `snapshot_first` before acting; its refusals `CHANGE_EMPTY`, `CHANGE_CONFLICTED` and `CHANGE_DIVERGENT` are this ticket's too.
- **R3.13**: "R3.17 pushes after a landing and refuses conflicted changes"; landing exports nothing to git.
- **ADR 0049 decision 12**: "The noun is `reef remote` … git-remote settings can live under `reef git` later." The global `--remote` flag names a remote *daemon*, so this ticket cannot use `--remote` for a git remote.

So this ticket builds:

1. **The push path**: a plan (range, checks, branch, lease), the push through jj-lib's `git::push_refs` off the repository thread, and a record (an operation that sets the local and remote-tracking bookmarks), in `reef_lib::jj::push` and the daemon's `publish` module. R3.17 reuses all three.
2. **The surface**: `GitService.PublishChange`, `reef publish`, an MCP tool `publish_change`, the pull-request hint for known hosts, and `--signoff` on `reef describe` and `reef publish`.
3. **The local-only rule**: publishing clears R1.16's `local` flag on the snapshots it pushed, and the reindex keeps it cleared.
4. **Discovery and measurement**: the root help, the guide, `reef here`'s hint, the quickstart, and onboarding scenario F.

### What was verified while writing this ticket

Reef at `ab6561d0` (R3.12 merged; API revision 27, `reef-api/src/lib.rs:49`; schema 16, migrations up to `0016_comments.sql`; ADR 0069 the latest; the threat model's last row S-165). PR #257 (R3.27) is in CI and takes API revision 28, schema 17, operation kind 16, `Operation` field 22 and ADR 0068. This ticket plans **API revision 29**, **ADR 0070**, threat rows **S-166 to S-176**, `OPERATION_KIND_PUBLISH = 17`, and **no migration**. If #257 has not merged when this ticket rebases, the numbers move down by one (revision 28, kind 16); whoever merges second renumbers. Paths are under `crates/` unless they start with `docs/`, `web/` or `sdk/`.

- **jj-lib 0.47.0 pushes through a git subprocess.** `jj_lib::git::push_refs(mut_repo, GitSubprocessOptions, remote, &GitPushRefTargets, callback, &GitPushOptions) -> GitPushStats` (`git.rs:3573`) validates the remote name (`validate_remote_name`, private, `:186`: no `/`, not `git`, a valid gix name), checks the remote is configured (`NoSuchRemote`), builds `--force-with-lease=refs/heads/<name>:<expected>` from each target's `before`, runs `git push --porcelain --no-verify … -- <remote> <oid>:refs/heads/<name>` (`git_subprocess.rs:234`–`:300`) with stdin on `/dev/null`, `LC_MESSAGES=C`, `-c core.fsmonitor=false`, and the fetch refspecs masked so the push updates no git tracking ref. On success it writes git's ref `refs/jj/remote-bookmarks/<remote>/<name>` itself (`to_fetched_ref_update`, private, with the private namespace `FETCH_BOOKMARK_REF_NAMESPACE`, `git.rs:111`) and sets the view's remote bookmark as `Tracked`. `GitPushStats` has `pushed`, `rejected` (a lease failed) and `remote_rejected` (with the remote's reason, `:214`). `push_updates` (`:3673`) pushes without touching the view or the fetched ref. `GitSubprocessOptions.environment` (`:153`) is "for environment variables like GIT_ASKPASS"; the command inherits the daemon's environment otherwise. `GitSubprocessError` distinguishes spawn failures, `UnsupportedGitOption` (git older than `MINIMUM_GIT_VERSION`, 2.42.0, `git_subprocess.rs:50`) and `External(stderr)`. `GitSubprocessCallback::remote_sideband` (`:741`) receives the remote's messages ("Create a pull request for … by visiting …" on GitHub and GitLab). The subprocess has no timeout and jj-lib keeps the `Child`.
- **Diffing fetched refs treats a remote bookmark missing from git's fetched namespace as deleted** (`diff_fetched_refs_to_import`, `git.rs:991`): a Reef publish that set the view's remote bookmark without git's `refs/jj/remote-bookmarks/…` ref would make the next `jj git fetch` delete it. Calling `push_refs` (which writes both) avoids reimplementing a private namespace.
- **Transactions cross threads.** `ReadonlyRepo::start_transaction(self: &Arc<Self>)` (`repo.rs:323`); the daemon's repository thread hands out `Arc<ReadonlyRepo>`, which is `Send + Sync` (`reef-daemon/src/repo/mod.rs`, ADR 0012). `MutableRepo::set_remote_bookmark` (`:1873`) and `set_local_bookmark_target` (`:1849`) exist; `git::get_all_remote_names(store)` (`git.rs:2710`) lists configured remotes.
- **Reef's git interop today** (`reef-lib/src/jj/git.rs`): `Repo::import_git_refs` (R1.05) before resolving a revision a client names, and `Repo::git_remote_head` (`:136`), the target of `refs/remotes/<remote>/HEAD`. No fetch, no export, no push anywhere. `[gc] git_executable` is passed to jj as `git.executable-path` (`reef-daemon/src/config.rs:1507`–`:1526`), so `GitSettings::from_settings(…).to_subprocess_options()` names the git the daemon already runs for `git gc`.
- **The daemon's environment is the first client's.** `autostart.rs` spawns `reef-daemon` detached, with stdio on `/dev/null`, its own process group and `/` as its directory, inheriting the starting CLI's environment (no `env_clear`). It is not a new session, so the starting terminal may still be its controlling terminal: a prompt from `git` or `ssh` could stop the daemon's child on `SIGTTIN` or wait forever.
- **Commits are authored by `[identity]`**, which defaults to `reef <reef@<hostname>>` (`config.rs:260`–`:276`), not the user's git identity. jj's CLI configuration (`git.private-commits`, `git.sign-on-push`, push-bookmark templates) is not in the settings Reef builds (`jj_settings_for`, `:1517`).
- **Descriptions keep their bytes.** `DescribeChange` stores the text as given, adding a trailing newline when missing and refusing a NUL (`normalize_description`, `reef-lib/src/jj/rewrite.rs:130`); `reef describe -m A -m B` joins paragraphs with a blank line as `jj describe` does (`cli/changes.rs:162`). Trailers survive, and git export writes the description as the commit message. jj-lib 0.47 has `trailer::parse_description_trailers` (`trailer.rs:60`). `DescribeChangeRequest` ends at field 5 (`changes.proto:302`) and never snapshots.
- **Snapshot-first helpers**: `uses_working_copy` (`service/reading.rs:274`) and `snapshot_first` (`:128`), as `TransitionChange` uses them (`service/changes.rs`, `transition_change`).
- **The local-only index.** `snapshots(op_id, repo_id, workspace_id, commit_id, change_id, kind, local DEFAULT 1, taken_at)` (`0003_snapshots.sql`), `index::for_change` (`snapshot/index.rs:217`), `index::counts` (`:130`), `reindex` (`:370`), which re-adds GC-rewritten operations as local. `no_writer_of_local_zero` (`reef-daemon/tests/snapshot_index.rs:421`) fails on any `local = 0` in `src/`. `ws inspect` and `ws list` report `local_snapshot_count`.
- **The proto.** `OperationKind` ends at `OPERATION_KIND_COMMENT = 15` and `Operation` at field 21, `comments` (`operations.proto`); `OperationEffects.bookmarks` already reports local and remote bookmark moves (`:271`), so a publish needs no `Operation` field. There is no `git.proto`. Reasons `CHANGE_EMPTY`, `CHANGE_CONFLICTED`, `CHANGE_DIVERGENT` (`reef-api/src/lib.rs:667`–`:673`), `IMMUTABLE_REVISION` (`:242`) and `OPERATION_NOT_UNDOABLE` (`:249`) exist.
- **Remote rules.** `remote/rules.rs` `TABLE` names every WRITE method; a test fails on a missing one. No rule refuses a remote caller outright today; ADMIN is never served remotely (ADR 0049 decision 6).
- **The CLI.** The global `--remote` (`cli/mod.rs:218`–`:229`, `REEF_REMOTE`) points a command at a remote daemon. `reef propose --to` names a destination bookmark. `out::shell_line(argv)` (`cli/out.rs:388`) quotes a command line. `reef here`'s `advise` (`cli/here.rs:124`) gives the in-workspace hint.
- **Budgets.** Root help 512 of 512 bytes (`cli/help.txt`; `publish` would not fit without removing words). Agent guide long form 1,597 of 1,600 (`cli/guide.rs:99`); the file 1,616 of 1,750. `tools/list` about 21,244 of 21,250 (`reef-vcs/tests/mcp.rs:209`, whose comment asks a raise to be argued in the brief). MCP server instructions 976 of 1,000.
- **The onboarding benchmark** (`docs/onboarding-benchmark.md`, `reef-testing/src/onboarding/`) has scenarios A to E, all on a clone of `BurntSushi/same-file` with no writable remote.
- **Local git** is 2.50.1 here; ubuntu-24.04 ships 2.43. Both are above jj-lib's minimum.

## Decisions

### Size, and the split (decision 1)

M at its top end. The work divides like this:

| Part | Size |
| --- | --- |
| reef-lib `jj/push.rs`: the plan (import, range, checks, branch rules, lease), the push on a throwaway transaction, the record transaction, the environment, stderr classification and redaction | about 650 lines |
| The daemon's `publish/` module: locks, snapshot first, describe and sign-off, the timeout, R1.16 marking, the host table, config `[publish]` | about 550 |
| `git.proto`, `DescribeChangeRequest.signoff`, `OPERATION_KIND_PUBLISH`, reasons, conversions, the remote rule, undo's refusal, reindex | about 300 |
| `reef publish`, `reef describe --signoff`, JSON and brief types, `reef schema` rows, help, guide, `here` | about 350 |
| MCP `publish_change` and the kind | about 100 |
| Onboarding scenario F (setup with a bare remote, prompt, judge) | about 250 |
| Web UI sentence and count | about 20 |
| `docs/publishing.md`, ADR 0070, the threat model, quickstart and other pages | prose, about 400 lines |

What stays out, each a follow-up issue the PR files (decision 1): **publishing gated by policy or checks** (decision 10), **stacked pull requests** (each change of a stack on its own branch, based on its parent's), **remote-listener tokens that may publish** (decision 14), and **forwarding the client's SSH agent** (decision 2). Any of the first two pushes this ticket to L.

### What publishing is

**Publishing** pushes one change's commit, and every ancestor the git remote lacks, to one branch on one remote configured in the repository's git config, and records it in jj's view: a local bookmark and a tracked remote bookmark at the pushed commit, in one operation of kind `publish`. Publishing again updates the same branch. It writes no lifecycle transition (decision 7) and opens no pull request: it returns the host's command for that.

The steps, for request revision `REV`, remote `R` and branch `B`:

1. **Gate**: `[publish] enabled` (else `PUBLISH_DISABLED`), a daemon `[identity]` that is not the default (else `IDENTITY_UNSET`, decision 9), a caller on the socket (else `REMOTE_PUBLISH_DENIED`), a repository with a git backend (else `NO_GIT_BACKEND`).
2. **Snapshot** `@` first when `REV` uses the workspace's working copy, unless `skip_snapshot` (R3.09's `uses_working_copy` and `snapshot_first`).
3. **Describe**, when `description` is non-empty or `signoff` is set: one describe operation through the existing write path (`Write::Describe`), with the sign-off applied (below). The commit that results is `C`.
4. **Plan**, a repository-thread job under the create lock shared, writing nothing but a git ref import: import git refs (R1.05, so a `git fetch` the user ran is seen), resolve `REV` to `C` (with `expected_commit_id`), choose `R` and `B`, compute the range, run the checks, classify the update and take the lease. With `dry_run` the response is built here and nothing else runs.
5. **Push**, under the branch's publish lock (below), off the repository thread, without the create lock: `push_refs` on a transaction started from the plan's `Arc<ReadonlyRepo>` and then dropped, so the remote, git's fetched ref and nothing else change.
6. **Record**, a repository-thread job under the create lock shared: set the remote bookmark `B@R` to `C` as tracked, set the local bookmark `B` (rules below), commit one operation, then clear R1.16's `local` on the pushed snapshots.

### The range and its checks

The **range** is `M = old..C` (jj's `::C ~ ::old`), where `old` is every target of a remote bookmark of `R` in the view after the import: the commits the remote's known branches lack, which is what jj's CLI checks before a push. It is built with `RevsetExpression::commits(old).range(&RevsetExpression::commits([C]))`, never parsed from text, and walked newest first.

Checks, each refusing with the first offending commit (newest first), its change id and commit id in metadata:

| Check | On | Reason (`FAILED_PRECONDITION`) | `next` |
| --- | --- | --- | --- |
| `C` is mutable and visible | `C` | `IMMUTABLE_REVISION`, `HIDDEN_REVISION` | none |
| `C` changes something against its parents | `C` | `CHANGE_EMPTY` | `reef publish @-` when `REV` was `@` and `@-` is not empty |
| a description whose first line is not blank | each commit | `CHANGE_UNDESCRIBED` | `C`: `reef publish <REV> -m <summary>`; an ancestor: `reef describe <change> -m <summary>` |
| no conflict | each commit | `CHANGE_CONFLICTED` | none; the hint says to resolve it |
| the change has one visible commit | each commit | `CHANGE_DIVERGENT` | `reef log -r 'change_id(<id>)'` |
| author and committer have a name and an email | each commit | `COMMIT_IDENTITY_MISSING` | none |
| author and committer are not the default `reef <reef@host>` form | each commit | `COMMIT_AUTHOR_DEFAULT` | `reef describe <change> --reset-author` |
| not another active workspace's working-copy commit | each commit | `INCLUDES_WORKING_COPY` (metadata `workspace`) | `reef publish --workspace <id> <REV>` |
| at most 1,000 commits | `M` | `RANGE_TOO_LARGE` (metadata `count`, "more than 1000") | none; the hint says to push trunk's history once with git |

"Another active workspace" excludes the request's own workspace: an agent publishes its own `@` (snapshotted in step 2), but publishing a change another agent is still writing would push half-written work (the design's rule, S-171). The cap stops a remote that has none of the repository's history from receiving all of it through a review branch.

Not checked: jj CLI's `git.private-commits` and `git.sign-on-push`, which are not in Reef's settings (stated in `docs/publishing.md`; a follow-up if asked).

### The remote and the branch (decisions 3 and 12)

- **Remote**: `git_remote` if given; else the only configured remote; else `origin` if configured; else `GIT_REMOTE_REQUIRED` (`FAILED_PRECONDITION`, metadata `remotes`). A given name must match `[A-Za-z0-9][A-Za-z0-9._-]{0,99}`, not be `git` (`INVALID_GIT_REMOTE`), and be one of `get_all_remote_names` (`GIT_REMOTE_NOT_FOUND`, `NOT_FOUND`, metadata `remotes`). `[publish] remotes`, when not empty, must list it (`GIT_REMOTE_NOT_ALLOWED`, `PERMISSION_DENIED`). A URL is never accepted as a remote.
- **Branch**: `branch` if given, else `<[publish] branch_prefix><first 12 characters of the change id in jj's letters>` (`reef/kxyzwqrsmtlo` with the default prefix `reef/`), which an amendment never changes. A branch must be 1 to 200 bytes of `[A-Za-z0-9._/-]`, not start with `-`, `.` or `/`, not end with `/`, `.` or `.lock`, contain no `//`, `..` or `@{`, not be `HEAD`, and parse as `gix::refs::FullName` once prefixed with `refs/heads/` (`INVALID_BRANCH`, `INVALID_ARGUMENT`). The prefix is checked with the same rules at config load (empty allowed).
- **Never trunk.** A branch named like `trunk()`'s bookmark, like the remote's HEAD branch (`git_remote_head(R)`), or whose remote bookmark on `R` points into `immutable()` is `BRANCH_NOT_PUBLISHABLE` (`FAILED_PRECONDITION`, metadata `why`: `trunk`, `remote_head` or `immutable`). Landing on `main` is `reef land` and R3.17, not a review branch.

**Bookmarks in jj's view.** The record sets the remote bookmark `B@R` (tracked) and a local bookmark `B`, as `jj git push --change` does. The user's `jj bookmark list` shows it, jj's CLI exports it to git on its next command, and the local bookmark follows later rewrites of the change, so `reef publish` (or `jj git push`) sends the amended commit to the same branch. Without a local bookmark a tracked remote bookmark reads as "deleted" to jj, and a later `jj git push --deleted` would delete the branch.

### Updates, the lease and `--force` (decision 4)

Let `T` be the view's target for `B@R` after the import, and `L` the local bookmark `B`.

| Case | Update | Allowed |
| --- | --- | --- |
| `T` absent | `CREATED` | yes; the lease is "must not exist" (jj's `to_git_lease` emits `refs/heads/B:`, `git.rs:339`), so a branch of that name Reef never fetched is `BRANCH_MOVED`, never overwritten |
| `T` is `C` | `UP_TO_DATE` | yes: the push still runs, with lease `C:C`, which only detects a remote that moved (one git process per no-op, accepted); no operation is written when the view does not change |
| `T` is an ancestor of `C` | `FAST_FORWARD` | yes |
| `change_id(T)` is one of `change_ids(M ∪ {C})` | `REWRITTEN` | yes: the branch was published from this work, and this is an amendment or rebase of it |
| anything else | `FORCED` | only with `force`; else `BRANCH_DIVERGED` (`FAILED_PRECONDITION`) |
| `T` conflicted (several targets) | none | `BRANCH_CONFLICTED`, hint `jj bookmark set` or `jj git fetch` |

**The same-change test** (decision 4, settled from an independent review): `REWRITTEN` holds when `T`'s change id is the change id of `C` or of a commit in `M`, never "a change in `::C`", which includes all of trunk and would let an unrelated change take over the branch of a long-landed one. `T` is always a commit in the view (the remote bookmark's target), so its change id comes from the local store: jj's git backend writes a `change-id` commit header (`git.write-change-id-header`, on in jj's defaults, which Reef's settings start from, `reef-lib/src/jj/settings.rs`) and reads it on import, falling back to an id derived from the commit hash. A commit a person made with plain git, or rewrote with `git commit --amend` (which drops the header), gets a derived id that matches nothing, so it reads as foreign, as it should. The test never uses predecessors or the evolog: they live in operations, which GC bounds; the change id is on the commit. ADR 0070 says so. No "earlier" test is needed: a visible `T` of `C`'s change other than `C` is already `CHANGE_DIVERGENT`, and an ancestor is `FAST_FORWARD`.

`BRANCH_DIVERGED` carries metadata `remote_commit_id`, `remote_change_id`, `remote_author` (name and email), `remote_summary` (the first line, sanitized as remote messages are) and `remote_hidden` (whether `T` is a commit Reef knows as rewritten), the hint "`R/B` is at <id> (<author>: <summary>), which is not part of this change. Rebase onto it, publish to another branch with `--branch`, or pass `--force` to replace it.", and `next` `reef show <T>` (until #259 gives a rebase).

`L` absent, at `T`, at `C`, or at a commit of `C`'s change, is moved to `C`; anything else is the same `BRANCH_DIVERGED` unless `force`, so `--branch feature` never silently moves a person's bookmark. In a colocated repository, a `B` that is git's checked-out branch (`HEAD` is the symbolic ref `refs/heads/B`) is `BRANCH_NOT_PUBLISHABLE` with `why` `checked_out`: jj's next export would move a person's checkout under them.

**The lease is always `T`**, Reef's last-known position (jj's `--force-with-lease`). A lease failure is `BRANCH_MOVED` (`FAILED_PRECONDITION`): "`R/B` moved since Reef last saw it", `next` `git -C <root> fetch <R> <B>` in a colocated repository (`jj -R <root> git fetch --remote <R> --branch <B>` otherwise), and the hint ends "then publish again; if the branch holds someone else's work you will be asked for `--force`". **The expected path over someone else's branch is three steps**, and it is the whole protection: `BRANCH_MOVED` → fetch → `BRANCH_DIVERGED` → `--force`; `docs/publishing.md` and ADR 0070 document it. A refusal by the remote (protected branch, declined hook, no permission to the branch) is `REMOTE_REJECTED` with the remote's reason in metadata `remote_reason`, sanitized as remote messages are.

**`force`** lifts only the same-change rule and the local-bookmark rule above: it may replace a branch holding another change's commit and move a person's local bookmark `B`. It never lifts the lease, and never makes trunk's branch, the remote's HEAD branch, an immutable branch or git's checked-out branch a target. `force` where it is not needed (`CREATED`, `FAST_FORWARD`, `REWRITTEN`, `UP_TO_DATE`) changes nothing and is not an error. A forced publish is recorded as such: `Publication.update` is `FORCED` and the operation's description ends " (forced)", so `reef op log` shows who replaced what.

### The push: credentials and environment (decision 2)

The push runs `[gc] git_executable` in the daemon's process, with the user's git configuration (credential helpers, `core.sshCommand`, `url.*.insteadOf`, `pushurl`) and the daemon's environment (`SSH_AUTH_SOCK`, `HOME`, `PATH`). Reef holds no git or host credential and never reads one. jj passes `--no-verify`, so no git hook runs.

`GitSubprocessOptions.environment` sets, for every push:

| Variable | Value | Why |
| --- | --- | --- |
| `GIT_TERMINAL_PROMPT` | `0` | git never prompts for a username or password |
| `GCM_INTERACTIVE` | `never` | Git Credential Manager never opens a window |
| `SSH_ASKPASS_REQUIRE` | `never` | ssh never runs an askpass program |
| `GIT_HTTP_LOW_SPEED_LIMIT`, `GIT_HTTP_LOW_SPEED_TIME` | `1000`, `60` | an HTTPS push slower than 1 KB/s for a minute fails |
| `GIT_TRACE`, `GIT_TRACE_CURL`, `GIT_TRACE_PACKET`, `GIT_CURL_VERBOSE` | `0` | no credential reaches stderr, which Reef reads and logs |
| `GIT_SSH_COMMAND` | the effective ssh command plus `-o BatchMode=yes -o ConnectTimeout=30 -o ServerAliveInterval=15 -o ServerAliveCountMax=4` | ssh never prompts and a dead connection ends; set only when `GIT_SSH` is unset and the effective command (`GIT_SSH_COMMAND` in the daemon's environment, else the repository's `core.sshCommand`, else `ssh`) has a program whose file name is `ssh`, so a `plink` or wrapper is left alone |

**Failures**, classified from git's stderr (English, since jj sets `LC_MESSAGES=C`), case-insensitively, first match wins:

| Reason | Kind | Matches |
| --- | --- | --- |
| `GIT_AUTH_FAILED` | `PERMISSION_DENIED` | `permission denied (publickey`, `authentication failed`, `could not read username`, `could not read password`, `terminal prompts disabled`, `host key verification failed`, `invalid username or password`, `returned error: 403`, `repository not found` |
| `GIT_UNREACHABLE` | `UNAVAILABLE` | `could not resolve host`, `connection refused`, `connection timed out`, `operation timed out`, `network is unreachable`, `no route to host`, `failed to connect` |
| `GIT_NOT_FOUND` | `UNAVAILABLE` | a spawn failure (`SpawnInPath`, `Spawn`): the hint names `[gc] git_executable` |
| `GIT_TOO_OLD` | `FAILED_PRECONDITION` | `UnsupportedGitOption` |
| `GIT_PUSH_FAILED` | `UNAVAILABLE` | anything else |

Every failure carries git's stderr, redacted (`scheme://anything@` becomes `scheme://***@`), control characters but newline and tab removed, the last 2,048 bytes, in metadata `git_stderr`. `GIT_AUTH_FAILED`'s hint says the push ran in the daemon's process and names what it had: "The push runs in the daemon (pid N) with its environment: SSH_AUTH_SOCK is unset. Load your key into an agent the daemon can reach, or configure a credential helper, then publish again; a daemon started from another shell has that shell's environment (`reef daemon stop`, then any reef command, restarts it from this one)." Metadata `ssh_auth_sock` is `set` or `unset`, never the path.

**The timeout and the lock.** A per-(repository, remote, branch) `tokio::sync::Mutex` is held from the plan to the record, so two publishes of one branch run one after the other. The push runs on `spawn_blocking`; the request waits at most `[publish] timeout` (default 5 minutes, 10 seconds to 1 hour), then returns `PUBLISH_TIMEOUT` (`UNAVAILABLE`). The thread cannot be killed (jj-lib owns the child), so it keeps the branch's lock; when git exits, a successful push is still recorded (logged at `info`), and a publish of the same branch meanwhile is `PUBLISH_IN_PROGRESS` (`UNAVAILABLE`) at once. The create lock is never held across the push, so a slow remote never holds back GC or another agent's writes, and the repository thread is never blocked on the network.

### The record (decisions 6 and 7)

The record job, on the repository thread under the create lock shared:

1. Refresh. If `C` is no longer visible (a snapshot amended it during the push), the local bookmark goes to the change's single visible commit instead, as it would have followed the rewrite; with none or several, the local bookmark is left as it is and a note says so. The remote bookmark is set to `C` either way: the push result is the remote's truth.
2. `set_remote_bookmark(B@R, RemoteRef { target: C, state: Tracked })`; `set_local_bookmark_target(B, …)` per the rule above.
3. Commit with `OpMeta` kind `publish`, description `reef: publish change <12-char id> to <R>/<B>`, and the caller's agent, actor and remote attributes as every write records them. No operation when nothing changed (an `UP_TO_DATE` publish with the local bookmark already there).

**No git export**, as for every Reef write (R3.13 decision 4): the jj CLI exports the local bookmark on its next command in a colocated repository; git's `refs/jj/remote-bookmarks/<R>/<B>` is already written by `push_refs`.

**Not undoable.** `UndoOperation` of a publish is `OPERATION_NOT_UNDOABLE`: "a publish pushed to R/B; publish again, or delete the branch on the host (`git push R --delete B`)". Undo of other operations is unchanged.

**The local-only rule (R1.16).** After the record, `snapshot::index::mark_published(db, repo, &commits) -> u64` sets `local = 0` on the rows whose `commit_id` is one of `M`'s commits (those the remote's branches lacked), and the response reports the count. Rows of earlier snapshots of the same change (its predecessors) stay local: git never sent them. `reindex` sets `local = 0` on the rows it adds when their commit is reachable from any remote bookmark (one `::remote_bookmarks()` containment test per added row, in the same repository job as its walk), so a GC rewrite never makes pushed work local again, and commits pushed by the jj CLI read as published after the next reindex. `no_writer_of_local_zero` becomes `local_zero_only_by_publish`: it allows those two functions in `snapshot/index.rs` and nothing else. No table records publications: the operation (with its bookmark effects) is the record, and GC's window bounds it like every other write's (decision 6).

### Description, trailers and sign-off (decision 8)

- **Trailers are kept.** `reef describe -m` already stores the text byte for byte (verified above); a test pins `Signed-off-by:` and `Co-Authored-By:` lines through describe, publish and the pushed git commit's message.
- **`signoff`** (`DescribeChangeRequest.signoff`, `PublishChangeRequest.signoff`, `--signoff` on both commands) appends `Signed-off-by: <[identity] name> <[identity] email>` as `git commit -s` does: to the trailer block if the last paragraph is one (by `jj_lib::trailer::parse_description_trailers`), else after a blank line; nothing when that exact trailer is present. With `signoff` and an empty `description`, `DescribeChange` keeps the current description and adds the trailer. Refused: `IDENTITY_UNSET` (`FAILED_PRECONDITION`) when `[identity]` is the default (name `reef`, email computed), since a sign-off certifies the DCO as a person; `SIGNOFF_WITHOUT_DESCRIPTION` when the description would be only the trailer. Reef never signs off unasked.
- **Author identity** (decision 9: the user chose to refuse, not warn). Publishing is refused until the daemon has a real identity:
  - **`IDENTITY_UNSET`** (`FAILED_PRECONDITION`, checked at the gate, before the snapshot): the daemon's `[identity]` is the default (name `reef`, email computed as `reef@<hostname>`). Hint: "Publishing pushes commits under the daemon's identity, which is the default reef <reef@host>. Set `[identity] name` and `email` in <config path>, then restart the daemon (`reef daemon stop`; the next reef command starts it)." No `next`.
  - **`COMMIT_AUTHOR_DEFAULT`** (`FAILED_PRECONDITION`, a row in the range checks, after `COMMIT_IDENTITY_MISSING`): a commit in `M` whose author or committer is the default identity's form (name `reef` and an email `reef@<any host>`), made before `[identity]` was set. Metadata `change_id`, `commit_id`; `next` `reef describe <change> --reset-author`.
  - **`reset_author`** (`DescribeChangeRequest.reset_author = 7`, `reef describe --reset-author`): rewrites the change's commit with author and committer set to the daemon's `[identity]` and the current time, keeping the description (or applying the new one), as `jj describe --reset-author` does; refused with `IDENTITY_UNSET` when `[identity]` is the default. `PublishChangeRequest` gets no such field: resetting an ancestor's author is a describe of that change, and the refusal's `next` names it.
  - Reef never rewrites authors unasked. The quickstart's publishing section says to set `[identity]` first.

### Checks before publishing (decision 10)

`reef publish` runs and requires no checks in this ticket. `docs/publishing.md` says plainly: git hooks never run (jj pushes with `--no-verify`, and Reef workspaces have no `.git`), so run the repository's checks before publishing (`reef job submit --wait` for `reef-checks.toml`'s, or the project's own command inside `reef ws run`). The follow-up issue is a repository-side gate: a `[publish]` table in `reef-policy.toml`, read from the destination as R3.13 reads it, evaluated by `PublishChange` exactly as `LandChange` evaluates, refusing anything but `ALLOW` with the same structured refusal. A flag the agent chooses would not be a gate.

### The pull-request hint (decision 13)

From `R`'s push URL as gix reads it (`pushurl`, else `url`, after `insteadOf` rewriting), with any userinfo dropped, the daemon derives `host` and `repository` (the path without a leading `/` or a trailing `.git`; each segment `[A-Za-z0-9._-]+`, else no command). `base` is `trunk()`'s bookmark name when `R` has a bookmark of that name, else `git_remote_head(R)`, else empty. `title` is the description's first line (trimmed, at most 256 characters); `body` is the rest after the first blank line, trailing whitespace trimmed.

| Host | `host_kind` | `argv` | `url` |
| --- | --- | --- | --- |
| `github.com`, or listed in `[publish] github_hosts` | `GITHUB` | `gh pr create --repo <repo> --head <B> [--base <base>] --title <title> --body <body>`, where `<repo>` is `owner/name` on github.com and `<host>/owner/name` elsewhere | `https://<host>/<repository>/compare/<base>...<B>?expand=1`, or `/pull/new/<B>` without a base |
| `gitlab.com`, or listed in `[publish] gitlab_hosts` | `GITLAB` | `glab mr create --repo <repo> --source-branch <B> [--target-branch <base>] --title <title> --description <body> --yes`, `<repo>` the path on gitlab.com and the `https://` URL elsewhere | `https://<host>/<repository>/-/merge_requests/new?merge_request%5Bsource_branch%5D=<B>` (and `target_branch`) |
| `bitbucket.org` | `BITBUCKET` | none | `https://bitbucket.org/<repository>/pull-requests/new?source=<B>&dest=<base>` |
| `codeberg.org` | `FORGEJO` | none | `https://codeberg.org/<repository>/compare/<base>...<B>` |
| anything else | `UNSPECIFIED` | none | none |

The implementer checks each flag against `gh pr create --help` and `glab mr create --help` of their current releases and drops a row's command (keeping its URL) if one is not documented. `--repo` and `--head` are both needed: the agent's directory is a Reef workspace with no `.git`, so `gh` cannot infer either. The response carries `argv` as an array; the CLI prints it shell-quoted (`shell_line`) as `next`, and so does the MCP tool. Without a command, the CLI prints the `url`, else the remote's own messages, which GitHub and GitLab already fill with a link.

**Remote messages** (`remote_sideband` lines) are returned as the remote's: control characters removed, at most 20 lines of 300 bytes each, never in `next`.

### Who may publish (decision 14)

- **On the socket**, any client: one uid is one trust domain (ADR 0027), and the client could run `git push` itself.
- **Through the remote listener**, nobody: `Rule::SocketOnly` (new), `REMOTE_PUBLISH_DENIED` (`PERMISSION_DENIED`), "publish from the daemon's machine". A `write` token today cannot reach the user's git credentials; publishing would hand them over, and a push can start the host's CI with its secrets. A token flag that allows it is a follow-up.
- **Operators** get `[publish] enabled` (default `true`) and `[publish] remotes` (default empty, every configured remote), for a harness sandbox that denies an agent the network but not Reef (S-166).

### Relationship to propose, land and R3.17 (decision 7)

- **No lifecycle transition.** Publishing to a git branch is review on the host, outside Reef's lifecycle; a change can be drafted, proposed or queued and published, in any order. R3.09's observed landing still records it `LANDED` if its commit reaches `trunk()` (a fast-forward or merge-commit merge on the host, then a fetch); a squash merge makes a new commit, which it does not see, as today.
- **One push path.** `reef_lib::jj::push` (plan, push, record) and the daemon's `publish` module are the only code that pushes. R3.17 calls them after a landing with a destination mode: the destination's own branch, fast-forward only, a conflicted change refused (already a check here), no pull-request hint. R4.14 replaces the git subprocess inside `push` with gitoxide and keeps the plan and the record. `dev-phases.md`'s R3.17 and R4.14 rows now say so.
- **The verb.** "Publish" is the design's word for a change leaving the machine. R4.07 publishes proposals to a store; it adds a store destination to the same command rather than a second verb.

### The API: `GitService`, API revision 29 (decision 11)

A new `git.proto`:

```proto
// A repository's git remotes (#258, ADR 0070; `docs/publishing.md`).
service GitService {
  // Pushes a change, and every ancestor the remote's branches lack, to a
  // branch on a git remote configured in the repository, and records the
  // local and remote-tracking bookmarks in one operation. Never through the
  // remote listener. Not idempotent: each call pushes.
  rpc PublishChange(PublishChangeRequest) returns (PublishChangeResponse) { ACCESS_WRITE }
}
enum PublicationUpdate { PUBLICATION_UPDATE_UNSPECIFIED = 0; PUBLICATION_UPDATE_CREATED = 1;
  PUBLICATION_UPDATE_FAST_FORWARD = 2; PUBLICATION_UPDATE_REWRITTEN = 3; PUBLICATION_UPDATE_FORCED = 4;
  PUBLICATION_UPDATE_UP_TO_DATE = 5; }
enum GitHostKind { GIT_HOST_KIND_UNSPECIFIED = 0;   // not a host Reef knows
  GIT_HOST_KIND_GITHUB = 1; GIT_HOST_KIND_GITLAB = 2; GIT_HOST_KIND_BITBUCKET = 3; GIT_HOST_KIND_FORGEJO = 4; }
message PullRequestHint { GitHostKind host_kind = 1; string host = 2; string repository = 3; string base = 4;
  repeated string argv = 5;            // empty: no command for this host
  string url = 6; }                    // empty: none
message Publication { string git_remote = 1; string branch = 2; string change_id = 3; string commit_id = 4;
  string previous_commit_id = 5;       // the branch's target before; empty when created
  PublicationUpdate update = 6;
  repeated string commit_ids = 7;      // the range M, newest first, at most 100
  uint32 commit_count = 8;
  repeated string change_ids = 9;      // M's changes, newest first, at most 100
  PullRequestHint pull_request = 10;   // unset for an unknown host
  repeated string remote_messages = 11;
  uint32 local_snapshots_published = 12;
  bool dry_run = 13; }
message PublishChangeRequest { string repo = 1; string workspace = 2;
  string revision = 3;                 // required; `@` needs `workspace`
  string git_remote = 4;               // empty: the only remote, else `origin`
  string branch = 5;                   // empty: `[publish] branch_prefix` and the change id's first 12 letters
  string expected_commit_id = 6;
  string description = 7;              // non-empty: set first, in a describe operation; never clears
  bool signoff = 8;                    // add the Signed-off-by trailer first
  bool force = 9;                      // replace a branch holding another change's commit, or move a person's local bookmark B; never past the lease, never trunk, the remote's HEAD, an immutable or a checked-out branch
  bool dry_run = 10;                   // plan and check only: no push, no operation
  bool skip_snapshot = 11; }
message PublishChangeResponse { Publication publication = 1;
  Operation operation = 2;             // the publish; unset for a dry run or when nothing changed
  Operation snapshot = 3; Operation describe = 4; Change change = 5;
  repeated string notes = 6; }         // sentences for a person, such as an UP_TO_DATE explanation
```

- `changes.proto`: `DescribeChangeRequest.signoff = 6` and `reset_author = 7` with their comments and errors. `operations.proto`: `OPERATION_KIND_PUBLISH = 17`; `OperationFilter.kinds` takes it.
- **Errors**, each documented on its RPC (ADR 0029): R2.02's revision reasons, `EXPECTED_COMMIT_MISMATCH`, `SNAPSHOT_FAILED`, `IMMUTABLE_REVISION`, `HIDDEN_REVISION`, `CHANGE_EMPTY`, `CHANGE_CONFLICTED`, `CHANGE_DIVERGENT`, `INVALID_DESCRIPTION`, and new: `INVALID_BRANCH`, `INVALID_GIT_REMOTE` (`INVALID_ARGUMENT`); `GIT_REMOTE_NOT_FOUND` (`NOT_FOUND`); `NO_GIT_BACKEND`, `GIT_REMOTE_REQUIRED`, `CHANGE_UNDESCRIBED`, `COMMIT_IDENTITY_MISSING`, `INCLUDES_WORKING_COPY`, `RANGE_TOO_LARGE`, `BRANCH_NOT_PUBLISHABLE`, `BRANCH_DIVERGED`, `BRANCH_CONFLICTED`, `BRANCH_MOVED`, `REMOTE_REJECTED`, `GIT_TOO_OLD`, `IDENTITY_UNSET`, `COMMIT_AUTHOR_DEFAULT`, `SIGNOFF_WITHOUT_DESCRIPTION` (`FAILED_PRECONDITION`); `PUBLISH_DISABLED`, `GIT_REMOTE_NOT_ALLOWED`, `REMOTE_PUBLISH_DENIED`, `GIT_AUTH_FAILED` (`PERMISSION_DENIED`); `GIT_UNREACHABLE`, `GIT_NOT_FOUND`, `GIT_PUSH_FAILED`, `PUBLISH_TIMEOUT`, `PUBLISH_IN_PROGRESS` (`UNAVAILABLE`). `DescribeChange` gains `IDENTITY_UNSET` and `SIGNOFF_WITHOUT_DESCRIPTION`.
- **Remote rule**: `("/reef.api.v1.GitService/PublishChange", Rule::SocketOnly)`, the variant documented as "refused to every remote caller".
- **ADR 0029's checklist** item by item in the PR; `API_REVISION` 29 (renumbered at rebase) with its digest. Not paged, so no `sdk/fixtures/page-items.json` entry. Both SDKs regenerate with no hand-written code.

### The CLI (decision 12)

```
reef publish [REV] [--branch NAME] [--git-remote NAME] [-m TEXT]... [--signoff]
             [--force] [--dry-run] [--expected-commit ID] [--no-snapshot]
reef describe [REV] [-m TEXT]... [--signoff] [--reset-author] [--expected-commit ID]
```

- `REV` defaults to `@`; outside a workspace `@` is R2.02's error, whose hint names `reef publish <change>`. `-m` joins paragraphs as `reef describe` does and becomes `description`. `reef describe` accepts `--signoff` without `-m`.
- **Text**: `published kxyzwqrs to origin/reef/kxyzwqrsmtlo (created, 1 commit)`, the notes, then `pull request: <url>` and `next: <command>` when there are any, else the remote's messages under `remote:`. A dry run says `would publish … (rewritten, 2 commits)` and lists the changes.
- **JSON** (`reef_client::json`): `Published { publication, operation, snapshot, describe, change, workspace, notes, next }`, where `next` is the shell-quoted `argv` or null; `Publication`, `PullRequestHint`. **Brief**: `publication.{git_remote, branch, commit_id, update}`, `notes` when non-empty, and `next`. `reef schema` gains the `publish` row and `describe`'s new flag.
- **Exit codes** by kind (ADR 0025): 0 published (or a dry run), 5 for `FAILED_PRECONDITION`, 7 denied, 6 unavailable, 3 not found, 2 usage.
- `reef op log` prints `publish change kxyzwqrs to origin/reef/kxyzwqrsmtlo`; `--kind publish` filters.

### MCP, SDKs, web UI, help, guide (decisions 15 and 16)

- **MCP**: a tool `publish_change` ("Publish change"; rpc `GitService/PublishChange`; destructive, not idempotent), with `repo`, `workspace`, `revision` (default `@`), `branch`, `git_remote` and `description`, description "Push a change to a branch on the repository's git remote, snapshotting its workspace first; publishing it again updates the branch. Returns {publication, operation, next}: next opens the pull request on a known host." It returns `Published` (brief). The kind enum of `list_operations` and `wait_for_operations` gains `publish`. **The budget argument** (`tests/mcp.rs`'s comment asks for one): publishing is the one write that takes an agent's work off the machine; without the tool an MCP agent leaves Reef for `git push` from the root, which is the workaround #194 records. The bound becomes the measured `tools/list` size rounded up to the next 50 bytes (about 22,200), and the comment records this ticket's reason.
- **Root help**: `publish` joins the "Also:" list after `describe`; `store` and `backup` leave it (both stay in `--help-all`), 507 of 512 bytes. `help_lists_every_command` gains `publish`.
- **Agent guide** (long form only; `AGENTS.md`'s short form is unchanged): one line after the Changes line, "`reef publish` pushes a change to a git branch; its `next` opens the pull request." The long form's budget rises from 1,600 to 1,700 bytes (to about 1,680); the file stays under 1,750.
- **`reef here`**: the in-workspace hints ("Work here; …") end with "`reef publish` sends its change to a git branch for review." (`text_fits_twelve_lines` still holds).
- **SDKs**: generated; their CI checks pass.
- **Web UI**: `words.ts` `[OperationKind.PUBLISH]: "published a change"`, and "published a change to <remote>/<branch>" from the operation's remote bookmark effect; `log/counts.ts` counts `PUBLISH` ("publish", "publishes").

### Configuration

A `[publish]` table in the daemon's `config.toml`, strict like the others, printed by `reef config show`:

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | `false` refuses every publish (`PUBLISH_DISABLED`) |
| `remotes` | `[]` | the remotes allowed; empty allows every configured remote |
| `branch_prefix` | `"reef/"` | the default branch's prefix; empty for none |
| `timeout` | `"5m"` | how long a request waits for the push, 10 s to 1 h |
| `github_hosts`, `gitlab_hosts` | `[]` | self-hosted hosts treated as GitHub or GitLab for the hint |

### Onboarding scenario F (decision 17)

| | Scenario | Setup | Harness | Prompt |
| --- | --- | --- | --- | --- |
| F | Publish | B's checkout (after `reef init`, as B runs it), with `origin` repointed at a bare clone of it in the trial's directory and fetched | as B: project files load | `f.txt`: "Append a line to README.md in an isolated workspace, then push your change to a new branch on this repository's git remote so it can be reviewed as a pull request. Print the branch name on your last line." |

The judge reads the bare remote: the last line must name a branch on it (other words allowed); that branch's commit has a non-empty description, its parent is the clone's base commit, and its `README.md` is the original with something appended (read with `git --git-dir <bare> cat-file`). "Calls to success" counts up to the first `reef publish` that exited 0; "Escaped to git" also counts `git push` and `jj git push`. F is held to B's targets. `--offline` builds the bare remote from the synthetic repository. The file remote has no known host, so F measures the push, not the hint, which the tests and the PR's own publication cover. The first CI dispatch's row starts F's baseline.

## Deliverables

### `crates/reef-api/`

- `proto/reef/api/v1/git.proto` (new) as above; `changes.proto` (`signoff`), `operations.proto` (`OPERATION_KIND_PUBLISH`), with comments and "Errors:" paragraphs; `src/lib.rs`: the reasons, `API_REVISION` 29 and its digest.

### `crates/reef-lib/`

- `src/jj/push.rs` (new): `PushPlan` and `Repo::plan_push(&self, &PushRequest) -> Result<PushPlan, PushError>` (import, range, checks, branch rules, update class, lease); `push(repo: Arc<ReadonlyRepo>, plan: &PushPlan, options: GitSubprocessOptions) -> Result<Pushed, PushError>` (a throwaway transaction, `push_refs`, a callback collecting remote messages); `Repo::record_push(&mut self, &PushPlan, &Pushed, &OpMeta) -> Recorded`; `push_environment(repo) -> HashMap<OsString, OsString>`; `classify(stderr) -> PushFailure`; `redact(&str) -> String`; `Repo::reachable_from_remotes(&[CommitId]) -> HashSet<CommitId>` for the reindex. `PushError` implements `Classify`.
- `src/jj/oplog.rs`: `OpKind::Publish` (`"publish"`); `classify` learns `reef: publish change`.
- `src/jj/rewrite.rs`: `with_signoff(description, name, email) -> Result<String, RewriteError>` beside `normalize_description`.

### `crates/reef-daemon/`

- `src/publish/` (new): `mod.rs` (the request flow, the branch locks, the timeout), `hosts.rs` (URL parsing, the host table, title and body), `identity.rs` (the default-identity test shared by the gate, the range check and `reset_author`).
- `src/service/git.rs` (new), `server.rs` (the service), `convert.rs` (`Publication`, `PullRequestHint`), `op_filter.rs` (`PUBLISH`), `error.rs` (the reasons), `service/changes.rs` (`signoff` on `DescribeChange`), `service/operations.rs` and the undo planner (publish refused), `remote/rules.rs` (`Rule::SocketOnly`, the row).
- `src/snapshot/index.rs`: `mark_published`; `reindex` derives `local`; module docs updated. `tests/snapshot_index.rs`: the renamed guard test.
- `src/config.rs`: `[publish]` and its validation; `config/tests.rs`.
- `README.md`: the publish operation's description.

### `crates/reef-client/`, `crates/reef-vcs/`

- `Client::publish_change`; JSON and brief types (`Published`, `Publication`, `PullRequestHint`).
- `src/cli/publish.rs` (new); `cli/changes.rs` (`--signoff`, `-m` optional with it); `cli/mod.rs` (the command); `cli/help.txt`; `cli/here.rs` (the hint); `cli/op.rs` (`publish`); `cli/schema/outputs.rs`; `mcp/tools.rs` and `mcp/args.rs` (the tool and the kind); `docs/agent-guide.md` and `cli/guide.rs` (the budget).

### `crates/reef-testing/`

- `src/onboarding/`: `Scenario::F`, its setup, judge and escape list; `onboarding/prompts/f.txt`; a judge fixture.

### `web/`

- `src/ui/words.ts`, `src/log/counts.ts`, with tests; the generated `src/gen/` code.

### Documentation

- **`docs/publishing.md`** (new): what publishing does and does not do; the branch name and `--branch`; what is pushed (the range) and every refusal with what to do; updates, the lease and `--force`; credentials (the daemon's environment, helpers, SSH agent, no prompts) and each failure; the pull-request hint per host and `[publish]`'s host lists; the remote's messages; trailers and `--signoff`; the author identity and `[identity]`; checks and hooks (none run; run them first); bookmarks in jj's view and how to delete a published branch; the local-only rule; the remote listener; what a squash merge on the host means for lifecycle state.
- **`docs/adr/0070-publishing-to-a-git-remote.md`** (accepted), indexed:
  - **Decisions**: the daemon pushes through jj-lib's `push_refs` on a throwaway transaction off the repository thread, then records on it; the user's git credentials in the daemon's environment, never a prompt; the branch from the change id with a local and a tracked remote bookmark; the range jj checks, with Reef's checks and the cap; the lease always, non-fast-forward only over the same work, `force` never past the lease, never trunk; not undoable; R1.16's flag cleared by publish and derived by reindex, no table; no lifecycle transition; one push path for R3.17 and R4.14; socket only; the host table and `argv`; `signoff` on describe and publish; no checks gate yet.
  - **Alternatives**: `push_updates` plus Reef writing jj's fetched-ref namespace; the push on the repository thread; running `git push` in the CLI; gitoxide now; forwarding the client's SSH agent; jj's `push-` prefix; an untracked remote bookmark only; force-with-lease everywhere; fast-forward only; a `publications` table; recording `PROPOSED`; any `write` token; opening the pull request through the host's API; a checks flag; an `Operation.publication` field.
- **`docs/threat-model.md`**: an asset row "Publishing (#258)": the repository's git remotes and the user's git credentials, used by the daemon's git subprocess; the local and remote-tracking bookmarks; the remote's branches. The Harness sandboxes section gains a paragraph (a sandboxed agent reaching the socket or `reef mcp` can push its change to a configured remote; deny the socket or set `[publish] enabled = false` or `remotes`). The remote listener requirements gain "`PublishChange` is never served remotely". Rows:
  - **S-166**, Network egress through the daemon, a sandboxed same-uid agent, Medium: an agent denied the network pushes code, or data written into a change, to a configured remote with the user's credentials. Documented and partly fixed: only remotes configured in the repository, never a URL; `[publish] enabled` and `remotes`; the Harness sandboxes paragraph. Tests: `publish::remotes`.
  - **S-167**, The user's git credentials through the remote listener, a `write` token, High: pushes from another machine, which can start the host's CI with its secrets. Fixed: `PublishChange` is refused to every remote caller. Tests: `publish::remote_listener`, the rules table.
  - **S-168**, Refspec and argument injection through branch or remote names, any client or a description, Medium: `+main`, `a:refs/heads/main`, `--upload-pack=…`, a URL as a remote. Fixed: the branch and remote rules above, the remote must be configured, and jj places `--` before the remote. Tests: `publish::remotes`, `jj_push::names`.
  - **S-169**, Credentials in responses, errors and logs, a `read` client or the log, Medium: a userinfo URL, git's stderr, git's trace variables. Fixed: userinfo dropped from every URL reported, stderr redacted and capped, the trace variables set to `0`, `SSH_AUTH_SOCK` reported as set or unset only. Tests: `jj_push::redaction`, `publish::auth_failure`.
  - **S-170**, Overwriting a branch someone else owns, an agent, Medium: a force push over a person's branch, or over commits a reviewer pushed. Fixed: the lease from the remote-tracking bookmark ("must not exist" when Reef has none); a non-fast-forward update only when the remote commit's change id is one of the published range's, unless `force`, which is recorded (`FORCED`, " (forced)" in the operation); never trunk, the remote's HEAD, an immutable branch or git's checked-out branch; a person's local bookmark is not moved without `force`. Tests: `jj_push::updates`, `jj_push::lease`.
  - **S-171**, Half-written work leaving the machine, an agent publishing another agent's change, Medium. Fixed: the caller's own `@` is snapshotted first; another active workspace's working copy in the range is refused; R1.16's flag is cleared only on what was pushed. Tests: `publish::refusals`, `publish::from_a_workspace`.
  - **S-172**, A push that prompts or hangs, the user's git setup or the network, Medium: a prompt on the daemon's terminal stops it, or a dead connection holds a thread forever. Partly fixed: prompts disabled for git, GCM and ssh, BatchMode and keep-alives for OpenSSH, HTTPS low-speed limits, the request's timeout, and the push never holds the repository thread or the create lock; a hung git cannot be killed (jj-lib owns it), so it holds its branch's lock until it exits. Tests: `publish::timeout`.
  - **S-173**, Repository-controlled git configuration, repository content, Low: an `include.path` the user added that points into the tree can set `core.sshCommand` or a credential helper, which the push runs. Documented, as S-13 documents `git gc`: the push is the user's git with the user's configuration; no hook runs (`--no-verify`).
  - **S-174**, Remote messages read by agents, the remote, Low: escape sequences or instructions in sideband text. Fixed: control characters removed, capped, labelled as the remote's, never in `next`. Tests: `jj_push::remote_messages`.
  - **S-175**, Injection through the pull-request command, a change's description, Medium: shell metacharacters in the title or body become part of `next`. Fixed: the command is an argv array, `next` is quoted by `shell_line`, and the host and repository path are restricted to `[A-Za-z0-9._-]` segments. Tests: `publish::hosts`.
  - **S-176**, A sign-off the person did not give, an agent, Low: an agent adds `Signed-off-by` in the configured person's name. Documented: only on an explicit `signoff`, never by default, refused with the default identity; `docs/publishing.md` says a sign-off certifies the DCO.
- **`docs/quickstart.md`**: a section "Publish a change for review" after "Look at what changed" (set `[identity]`; `reef publish -m …` from inside `reef ws run`; run `next`), and a limitation line (no hooks run; credentials come from the daemon's environment). **`docs/onboarding-benchmark.md`**: scenario F's row, prompt, judge and escapes. **`docs/design.md`**: the git interop paragraph links `docs/publishing.md` and ADR 0070 as the push before R3.17. **`docs/landing.md`**: the "a local `main` and its git remote" paragraph points pull-request teams at `reef publish`. **`docs/agent-guide.md`**, as above.
- **`README.md`** "Phase 3 so far": a bullet for publishing, API revision 29. **`CHANGELOG.md`** Unreleased: Added (`GitService.PublishChange`, `reef publish`, the `publish_change` tool, `--signoff`, `[publish]`, onboarding scenario F); Changed (the root help's "Also:" list, the guide, `reef here`'s hint; R1.16's `local` is cleared for published commits).
- **`docs/dev-phases.md`** (R3.17 and R4.14) and this directory's README: done with this brief.

## Tests

In `crates/reef-lib/` (`tests/jj_push.rs`, against a bare remote made with `git init --bare` and pushed to through `file://`; each test prints a skip note and returns when `git` is not on `PATH`):

1. `plan`: the default branch; the range for one change, a stack of two, and a change whose parent is already on the remote; every refusal in the checks table (the cap lowered through a test hook), `BRANCH_NOT_PUBLISHABLE` for trunk's name, the remote's HEAD and an immutable branch, `BRANCH_CONFLICTED`.
2. `updates`: created, fast-forward, rewritten (amend, then plan; and a rebase of the change), forced with `force` (recorded `FORCED` and " (forced)") and `BRANCH_DIVERGED` without (with `remote_author`, `remote_summary` and `remote_hidden`), up to date, `force` where not needed is a no-op; a branch whose remote commit is an old commit of a landed change on trunk is `BRANCH_DIVERGED`, not `REWRITTEN`; a remote commit made with plain git is foreign; a branch on the remote that Reef never fetched is `BRANCH_MOVED`; git's checked-out branch in a colocated repository is `BRANCH_NOT_PUBLISHABLE`; a person's local bookmark at an unrelated commit refused without `force`.
3. `push_and_record`: the bare remote has `refs/heads/reef/<id>` at `C`; the view has `reef/<id>` and tracked `reef/<id>@origin` at `C`; the operation's kind and description; afterwards `import_git_refs` and a jj-lib `GitFetch` of `origin` with its import change nothing and leave no bookmark conflicted.
4. `lease`: the bare remote's branch moved from another clone: `BRANCH_MOVED`, the view unchanged; a `pre-receive` hook in the bare remote that declines: `REMOTE_REJECTED` with the hook's message.
5. `rewritten_during_push`: `C` rewritten between the push and the record: the local bookmark is on the successor and the change is not divergent.
6. `environment`: a fake `git` (a script set as `[gc] git_executable` that records its environment and arguments, then runs the real git) sees every variable in the table, `--no-verify`, and the remote after `--`; the `GIT_SSH_COMMAND` rule for `ssh -i key`, for `plink`, and with `GIT_SSH` set.
7. `classification`, `redaction`, `remote_messages`, `names`: canned stderr per reason; userinfo URLs; control characters and caps; the branch and remote rules on `+main`, `a:b`, `-x`, `--upload-pack=x`, `HEAD`, `..`, `@{u}`, `x.lock`, a space, a URL, `git`, a 201-byte name.
8. `signoff`: appended after a trailer block, after a body, not duplicated; `Co-Authored-By` and existing `Signed-off-by` lines kept byte for byte.

In `crates/reef-daemon/` (`tests/publish.rs` through `TestDaemon` with `testing::no_automatic_snapshots` and a bare remote):

9. `from_a_workspace`: an edited `@` is snapshotted, described with `description`, pushed and recorded; `snapshot`, `describe` and `operation` in the response; `ListOperations` shows kind `PUBLISH` with both bookmark effects; the snapshot rows of `C` are `local = 0`, earlier snapshots of the change stay local, and `ws inspect`'s local count drops by `local_snapshots_published`.
10. `publish_again_updates`: an edit, then a publish: `REWRITTEN`, the same branch, the remote at the new commit; a publish with nothing new: `UP_TO_DATE` and no operation.
11. `refusals`: undescribed (both `next` forms), empty (`next` `reef publish @-`), conflicted, divergent, another workspace's working copy (`next` with `--workspace`), each with its hint.
12. `signoff_and_trailers`: `DescribeChange` and `PublishChange` with `signoff`; the pushed commit's message read from the bare remote has every trailer unchanged; `IDENTITY_UNSET` with the default `[identity]`; `SIGNOFF_WITHOUT_DESCRIPTION`; `PublishChange` with the default `[identity]` is `IDENTITY_UNSET`; a commit authored as `reef <reef@host>` before `[identity]` was set is `COMMIT_AUTHOR_DEFAULT`, and `reef describe --reset-author` clears it.
13. `dry_run`: the predicted update and range, the bare remote unchanged, no operation, no row changed.
14. `remotes`: the only remote by default; `origin` among several; `GIT_REMOTE_REQUIRED`; `GIT_REMOTE_NOT_FOUND` listing remotes; `[publish] remotes` and `enabled = false`; `INVALID_BRANCH` and `INVALID_GIT_REMOTE` for the names in test 7.
15. `remote_listener`: a `write` token gets `REMOTE_PUBLISH_DENIED`; a `read` token is refused by scope.
16. `timeout`: a fake git that sleeps; with `[publish] timeout = "10s"` the request is `PUBLISH_TIMEOUT`, a second is `PUBLISH_IN_PROGRESS`, and when the fake exits successfully the operation appears.
17. `auth_failure`: a fake git printing `Permission denied (publickey).` and exiting 128: `GIT_AUTH_FAILED`, metadata `ssh_auth_sock`, `git_stderr` redacted.
18. `not_undoable`: undo of a publish is `OPERATION_NOT_UNDOABLE` with its hint.
19. `reindex_keeps_published`: after GC rewrites the operations, the reindexed rows of pushed commits are `local = 0`; `local_zero_only_by_publish` passes.
20. `hosts` (unit tests in `publish/hosts.rs`): github.com over `https`, `ssh://` and `git@host:path` forms; a GHE host listed in `github_hosts` (`--repo host/owner/name`); GitLab subgroups and a self-hosted URL; Bitbucket and Codeberg URLs; an unknown host; userinfo dropped; base from trunk, from the remote HEAD, and none; title and body split, a quote and newlines in both, quoted by `shell_line`; a path segment with other characters gives no command.

In `crates/reef-vcs/` (`tests/cli.rs`, `tests/schema.rs`, `tests/mcp.rs`):

21. `publish_cli`: `reef publish` from inside a workspace (`-m`, `--signoff`, `--branch`, `--git-remote`, `--dry-run`, `--force`) in text, `-o json`, `-o brief` and piped, validated against `reef schema`; exit codes for a refusal and an unavailable remote; `next` rendered from a canned github hint; `reef describe --signoff` without `-m`; `reef op log --kind publish`; the root help golden file and its budget; the guide's budget at 1,700; `reef here`'s hint; `publish_change` in `tools/list` under the new bound, its result validated against its output schema; `publish` in both kind enums.

In `crates/reef-testing/`: scenario F's judge on fixtures (a branch with the appended README passes; a missing branch, an unchanged README or an empty description fails); `reef-onboarding run --scenario F --offline` sets up and tears down the bare remote. In `web/` (vitest): the sentence with and without a remote bookmark effect, and the count. In `crates/reef-api/`: the digest at revision 29; `just api` and `just api-breaking`.

## Implementation notes

- **Order**: reef-lib's names, plan and checks (tests 1, 2, 7); push and record against a bare remote (3, 4, 5); the environment and classification (6, 7); sign-off (8); the API, the service and the daemon flow (9 to 11, 13, 14); the timeout, auth and undo (16 to 18); R1.16 (9, 19); the remote rule (15); the host table (20); the CLI, schema, MCP, help and guide (21); the web UI; scenario F; the documents.
- **Threads and locks**: the plan and the record are repository-thread jobs, each under the create lock shared; the push is `spawn_blocking` with only the branch lock held; SQLite (R1.16) on the async side's blocking pool after the record, never inside the repository job.
- **The throwaway transaction** must be started from the plan's `Arc<ReadonlyRepo>` inside the blocking closure, so no non-`Send` jj value crosses a thread, and must be dropped, never committed: the record transaction on the repository thread is the only operation a publish writes.
- **Verify jj CLI's range check** against jj-cli 0.47.1's `git push` (its checks for description, author and committer, conflicts) and say in the PR whether Reef's range differs. Reef does not depend on jj-cli.
- **Dogfood**: publish this ticket's own PR branch with the new `reef publish` from its Reef workspace and open the PR with its `next`; log the friction in the PR, as the runbook asks.
- **Linux**: nothing is `cfg(target_os = "linux")`, but the daemon's Linux build, clippy and `tests/publish.rs` run in the privileged container before pushing (git is in the image).

## Performance expectations

Release build, M2 Max, a local bare remote; trend data, no gate:

| Measurement | What | Expect |
| --- | --- | --- |
| `PublishChange`, created | a 10-file change, one commit | < 300 ms, most of it git's process |
| `PublishChange`, dry run | the same | < 30 ms |
| The plan's range and checks | a 100-commit range | < 20 ms |
| R1.16 marking | 100 commits | < 5 ms |

## Acceptance criteria

- [ ] From inside `reef ws run`, an agent with no jj knowledge gets a change onto a remote branch with one command (`reef publish -m …`), and the `next` it prints opens the pull request on GitHub (tests 9, 20, 21; scenario F; the PR's own publication).
- [ ] Publishing an amended change updates the same branch; nothing new is a no-op (test 10).
- [ ] Empty, conflicted, divergent, undescribed and half-written work is refused with `hint` and `next` (test 11); trunk and branches Reef did not publish are protected, and the lease always applies (tests 2, 4).
- [ ] Trailers survive describe and publish byte for byte; `--signoff` adds one, only when asked (tests 8, 12).
- [ ] Credentials stay the user's git's; no prompt, no secret in responses or logs; failures read as auth, network or other (tests 6, 7, 16, 17).
- [ ] Never through the remote listener; operators can disable it or limit remotes (tests 14, 15).
- [ ] R1.16's `local` is cleared for pushed snapshots and stays cleared through GC (tests 9, 19).
- [ ] API revision 29; `reef publish`, `reef describe --signoff`, `publish_change`; root help, guide and `here` show it; quickstart and onboarding scenario F (test 21).
- [ ] ADR 0070, `docs/publishing.md`, the threat model and the other documents updated; `just ci` and `just doc` pass; the Linux container's build, clippy and publish tests pass.

## Verification

```sh
export CARGO_TARGET_DIR=/Users/arsood/Projects/ReefVCS/target-258 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only
cargo nextest run -p reef-lib --test jj_push
cargo nextest run -p reef-daemon --test publish --test snapshot_index --test api_v1
cargo nextest run -p reef-vcs --test cli --test schema --test mcp
cargo nextest run -p reef-testing onboarding
cargo nextest run -p reef-api
(cd web && npm test)
# By hand, in a scratch repository with a bare remote:
git init --bare /tmp/remote.git && git remote add origin /tmp/remote.git && git push origin main
reef ws run -- sh -c 'echo note >> README.md && reef publish -m "Add a note"'
git --git-dir /tmp/remote.git log --format='%s%n%b' reef/<id>
just doc && just ci
```

## Out of scope

- **A checks or policy gate before publishing** (decision 10): the follow-up's `[publish]` table in `reef-policy.toml`.
- **Stacked pull requests**: each change of a stack on its own branch with `--base` its parent's branch (a follow-up beside R3.10).
- **Publishing through the remote listener** (decision 14) and **forwarding the client's SSH agent** (decision 2): follow-ups.
- **Fetching and rebasing** (`reef rebase`, #259), and deleting a published branch (`git push --delete`, documented).
- **Git inside workspaces** (#194's questions 1 and 3): `git status`, hooks and tooling in a workspace.
- **Opening the pull request** through a host's API: Reef holds no host credentials.
- **jj CLI settings** Reef's settings lack: `git.private-commits`, `git.sign-on-push`, push-bookmark templates.
- **Native gitoxide push** (R4.14) and **landed history** (R3.16, R3.17).
- **A record of publications in `reef.sqlite3`** (decision 6).

## Risks

- **Classifying git's stderr is brittle.** Mitigated: the unmatched case is `GIT_PUSH_FAILED` with the redacted stderr, so a new message degrades the reason, not the answer; the patterns are in one table with tests.
- **The daemon's environment decides credentials.** A daemon started from a shell without the SSH agent fails over SSH while `git push` in the user's terminal works. Mitigated by the hint, which names what the daemon lacks and how to restart it; forwarding is a follow-up.
- **An uncancellable push thread** holds one blocking thread and its branch's lock until git exits. Mitigated by ssh keep-alives, HTTPS low-speed limits and `PUBLISH_IN_PROGRESS`.
- **Two budgets rise** (`tools/list` by about 950 bytes, the guide by about 80). Decisions 15 and 16 give the alternatives.
- **`gh` and `glab` flags drift.** Mitigated: the flags are checked at implementation, the URL is always given, and the table is one function.
- **Field and kind numbers** collide with #257's at rebase; renumbered then.

## Choices this brief made, and why

- **`push_refs` on a throwaway transaction, then a record on the repository thread**, because jj-lib writes its private fetched-ref namespace only inside `push_refs`, and the repository thread must never wait on the network.
- **The lease always, and non-fast-forward only over the same work**, because an agent that can force-push any branch Reef has seen can erase a person's branch or a reviewer's commits.
- **No table**, because the operation and its bookmark effects already record the publish, and R1.16's flag can be derived from the remote bookmarks after GC; a table would add a migration for audit beyond GC's window, which no other write has.
- **Socket only**, because a remote token must not become the user's git credentials.
- **An argv in the response, `next` quoted by the client**, because a description is untrusted text and every client (CLI, MCP, SDK) needs the same safe command.
- **No lifecycle transition**, because review on the host is not Reef's review, and R3.09's observed landing already sees what reaches `trunk()`.

## What later tickets inherit

- **R3.17** calls `plan_push`, `push` and `record_push` with a landing mode: the destination's branch, fast-forward only, no hint; conflicted changes are already refused.
- **R4.14** replaces the subprocess inside `push` with gitoxide; the plan, the lease, the record, the classification's reasons and the environment's intent stay.
- **R4.07** adds a store destination to `reef publish`; R1.16's flag means "left the machine" for both.
- **R3.10** (or its follow-up) publishes a stack as one branch per change, with `--base` the parent's branch from this ticket's hint.
- **#259** (`reef rebase`) and a later `reef fetch` make `BRANCH_MOVED`'s `next` a `reef` command.
- **The checks-gate follow-up** reuses R3.13's evaluation inside `PublishChange`.

## Departures from dev-phases.md

This issue has no row. `dev-phases.md` and this directory's README were edited to match with this brief:

- **R3.17 pushes through this ticket's push path**: after a local landing it pushes the destination through the same plan, lease and record, fast-forward only onto the destination's branch, refusing a conflicted change, rather than building a second push.
- **R4.14 hardens both pushes**: this ticket delivers the first push (`reef publish`, through jj-lib's git subprocess and the user's git credentials), and R3.17 the first push of landed history; R4.14 makes them native.

## Decisions for the user

Answered by the user on 2026-11-09. Every recommendation below was accepted except two: **decision 4** was settled by an independent review the user asked for, which kept the recommendation and tightened it (the same-change test on `M ∪ {C}`'s change ids, the "must not exist" lease, the three-step path, a recorded `--force`, the checked-out branch; see "Updates, the lease and `--force`"); and **decision 9** is the alternative "refuse until `[identity]` is set" (`IDENTITY_UNSET`, `COMMIT_AUTHOR_DEFAULT` and `reset_author`; see "Description, trailers and sign-off"). The list is kept as asked.

**Group 1: scope**

1. **Size and split.** M at its top end, with four follow-up issues the PR files: a checks or policy gate before publishing, stacked pull requests, remote-listener tokens that may publish, and forwarding the client's SSH agent. *Recommended*: as stated. Alternatives: the policy gate in this ticket (L; decision 10); or drop onboarding scenario F to a follow-up (M, but the issue's acceptance names it).

**Group 2: how the push works**

2. **Credentials.** The daemon pushes through jj-lib's `push_refs` (the `git` subprocess, `[gc] git_executable`), with the user's git configuration and credential helpers and the daemon's environment (its `SSH_AUTH_SOCK`); every prompt is disabled, OpenSSH runs in BatchMode with keep-alives, and a failure is `GIT_AUTH_FAILED`, `GIT_UNREACHABLE` or `GIT_PUSH_FAILED` with redacted stderr and a hint naming what the daemon's environment lacks. *Recommended*: as stated. Alternatives: the CLI sends its `SSH_AUTH_SOCK` with each request on the socket and the daemon uses it for that push (fixes a daemon started from another shell; adds a per-request environment surface); run `git push` in the CLI's process (breaks "the daemon does the push", and a workspace has no `.git`); gitoxide now (R4.14, L).
3. **Branch name and bookmarks.** `reef/<first 12 letters of the change id>` (`[publish] branch_prefix`, `--branch` overrides), stable across amendments, recorded as a local bookmark plus a tracked remote bookmark, as `jj git push --change` does. *Recommended*: as stated (the prefix groups Reef's branches on the host and keeps clear of hand-made `push-` bookmarks). Alternatives: jj's own `push-<id>` default, so `jj git push -c` and `reef publish` share a branch; an untracked remote bookmark and no local bookmark (nothing in `jj bookmark list`, but nothing follows amendments either); a per-user prefix such as `<user>/`.
4. **Update semantics.** The lease is always Reef's last-known remote position; a non-fast-forward update is allowed only over an earlier commit of work being published (an amendment); `--force` lifts only that rule, never the lease; trunk's branch, the remote's HEAD branch and immutable branches are never published to. *Recommended*: as stated. Alternatives: jj's behavior (force-with-lease over any branch Reef has seen, so `--branch feature` overwrites a person's branch); fast-forward only (every amendment needs a new branch and a new pull request).
5. **What is pushed and checked.** `C` and every ancestor the remote's branches lack (jj's range), each checked for a description, conflicts, divergence, author and committer, and another live workspace's working copy; `C` must not be empty; at most 1,000 commits; `@` snapshotted first. *Recommended*: as stated. Alternatives: refuse a range with more than one change, so stacks are published one change at a time; allow another workspace's working copy in the range with a note.

**Group 3: records and relationships**

6. **R1.16 and the record.** Publishing clears `local` on the pushed commits' snapshot rows; the reindex derives `local = 0` from remote bookmarks so GC never reverts it; the operation (kind `publish`, with its bookmark effects) is the only record; publishes are not undoable. *Recommended*: as stated (no migration). Alternatives: a `publications` table (schema 18) recording who published which commit to which branch, beyond GC's window; carry the flag by commit id through the reindex instead of deriving it (no jj-CLI pushes counted).
7. **Propose, land and R3.17.** Publishing writes no lifecycle transition; `reef_lib::jj::push` and the daemon's `publish` module are the one push path, which R3.17 reuses fast-forward onto the destination's branch (its row edited) and R4.14 makes native. *Recommended*: as stated. Alternatives: publishing records `PROPOSED` (a pull request is a proposal, but Reef's review and the host's would then share a state); or build R3.17 first and publish on top of it.

**Group 4: what the commits say**

8. **Trailers and sign-off.** `reef describe` already keeps trailers byte for byte (verified; a test pins it); `--signoff` on `reef describe` and `reef publish` (`signoff` on both requests) adds `Signed-off-by: <[identity]>` once, refused with the default identity or an empty description, never added unasked. *Recommended*: as stated. Alternatives: `[publish] signoff = true` to sign off every publish; sign off with git's `user.name` and `user.email` instead of `[identity]`; no option (the runbook types the trailer).
9. **Author identity.** Reef does not rewrite authors; a publish whose commits are authored by the default `reef <reef@host>` carries a note to set `[identity]`. *Recommended*: as stated. Alternatives: refuse until `[identity]` is set; rewrite those authors to `[identity]` before pushing (new commits, which workspaces then follow); default `[identity]` from git's configuration (a separate issue, since it changes every commit Reef writes).
10. **Checks before publishing.** None in this ticket; the docs say git hooks never run and to run checks first; the follow-up is a `[publish]` table in `reef-policy.toml` evaluated as `LandChange` evaluates. *Recommended*: as stated. Alternatives: a `--require-allow` flag now (the agent chooses it, so it gates nothing); the policy gate now (about 300 more lines and a policy format addition, L); run `reef-checks.toml` through the executor and wait (needs a running executor on every machine that publishes).

**Group 5: the surface**

11. **API.** A new `GitService.PublishChange` (WRITE) in `git.proto`, with `description`, `signoff`, `force` and `dry_run`, returning a `Publication` with the range, the update class, the pull-request hint and the remote's messages; `OPERATION_KIND_PUBLISH`; `DescribeChangeRequest.signoff`; API revision 29. *Recommended*: as stated. Alternatives: the RPC on `ChangeService` beside `TransitionChange`; a `ListGitRemotes` READ as well; no `description`/`signoff` on the request (clients describe first, and an MCP agent makes two calls).
12. **CLI flags.** `reef publish [REV] [--branch] [--git-remote] [-m] [--signoff] [--force] [--dry-run]`. `--remote` is the global remote-daemon flag (ADR 0049) and cannot be reused. *Recommended*: `--git-remote`. Alternatives: `--to REMOTE` (but `reef propose --to` names a bookmark); a positional remote as in `git push origin` (ambiguous with `REV`).
13. **The pull-request hint.** A host table: github.com and `[publish] github_hosts` give `gh pr create --repo … --head … --base … --title … --body …`; gitlab.com and `gitlab_hosts` give `glab mr create …`; Bitbucket and Codeberg a URL only; other hosts nothing but the remote's own messages. Title and body come from the description; the response carries `argv`, quoted into `next` by the client. *Recommended*: as stated. Alternatives: GitHub only; URLs only, no command; `gh pr create --fill` (fails in a workspace, which has no `.git`).
14. **Who may publish.** Any client on the socket; nobody through the remote listener (`REMOTE_PUBLISH_DENIED`); `[publish] enabled` and `remotes` for operators. *Recommended*: as stated. Alternatives: any `write` token, as for `LandChange`; a new token flag or scope that allows publishing (an API and token-store change).
15. **MCP.** A `publish_change` tool, with `tools/list`'s bound raised to its measured size rounded up to 50 (about 22,200 bytes), argued in the test's comment. *Recommended*: as stated (the issue asks for it, and it is the one write that takes work off the machine). Alternative: the RPC in `NOT_TOOLS` (MCP agents run `reef publish` in a shell; the budget stays at 21,250).
16. **Discovery.** The root help's "Also:" list gains `publish` and drops `store` and `backup` (both stay in `--help-all`); the guide's long form gains one line with its budget raised from 1,600 to 1,700 bytes; `reef here`'s in-workspace hint names `reef publish`; `AGENTS.md`'s short form is unchanged. *Recommended*: as stated. Alternatives: keep 1,600 by trimming (the measured cuts rewrite `ws list`'s "counts changed files", which the token benchmark relies on); put `reef publish` in the short form, which every agent reads (about 75 bytes in every `AGENTS.md`); `--help-all` only.
17. **Onboarding.** A new scenario F: B's setup with `origin` repointed at a local bare clone, prompted to push the change to a branch for review, judged from the bare remote. *Recommended*: as stated. Alternatives: a publish step added to scenario B (no new scenario, but B's baseline restarts); no benchmark change (the issue's acceptance unmet).
