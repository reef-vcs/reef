# Publishing a change to a git remote

`reef publish` sends a change to a branch on one of the repository's git remotes, so it can be reviewed as a pull request on the host. It is how work leaves the machine before the landing service pushes landed history itself (R3.17). The decision record is [ADR 0070](adr/0070-publishing-to-a-git-remote.md); the RPC is `GitService.PublishChange`, and MCP's tool is `publish_change`.

```sh
reef ws run -- sh -c 'echo note >> README.md && reef publish -m "Add a note"'
# published kxyzwqrs to origin/reef/kxyzwqrsmtlo (created, 1 commit)
# pull request: https://github.com/owner/repo/compare/main...reef/kxyzwqrsmtlo?expand=1
# next: gh pr create --repo owner/repo --head reef/kxyzwqrsmtlo --base main --title 'Add a note' --body ''
```

Run `next` to open the pull request. Reef holds no host credentials and opens nothing itself.

## Before the first publish

Set the daemon's identity. Commits Reef writes are authored and committed by `[identity]` in the daemon's `config.toml`, and the default, `reef <reef@host>`, is not a person a host can attribute work to, so publishing refuses it (`IDENTITY_UNSET`):

```toml
[identity]
name = "Pat Example"
email = "pat@example.com"
```

Then restart the daemon (`reef daemon stop`; the next `reef` command starts it). A commit written before the identity was set is refused with `COMMIT_AUTHOR_DEFAULT`; `reef describe <change> --reset-author` rewrites it with the new identity.

## What a publish does

1. **Snapshots** the workspace's working copy when the revision uses it (`@` by default), unless `--no-snapshot`.
2. **Describes** the change when `-m` or `--signoff` is given, in its own describe operation. A publish refused after this step keeps the description (and the sign-off): fix what it says and publish again.
3. **Plans**: imports git refs (so a `git fetch` you ran counts), resolves the change, picks the remote and branch, works out what the remote lacks, runs the checks below, and decides how the branch moves. `--dry-run` stops here and reports the plan; it snapshots and describes nothing.
4. **Pushes** the change's commit, and every ancestor the remote's branches lack, with the daemon's git (`[gc] git_executable`).
5. **Records** the publish in one operation of kind `publish`: a local bookmark and a tracked remote bookmark of the branch at the pushed commit, as `jj git push --change` leaves them. `reef op log --kind publish` lists them, and `jj bookmark list` shows the bookmarks.

Publishing again updates the same branch: an amended change replaces its earlier commit there, and a change with nothing new is `up-to-date` and writes no operation. Publishing writes no lifecycle state: it is review on the host, and a change can be drafted, proposed or published in any order. If the host merges it as a fast-forward or a merge commit and the merge is fetched, the change is recorded as landed when it reaches `trunk()`, as any commit is ([proposals](proposals.md)); a squash merge makes a new commit, which Reef does not connect to the change.

## The branch and the remote

- **The branch** is `--branch NAME`, or by default `[publish] branch_prefix` (`reef/`) and the first 12 letters of the change id in jj's form: `reef/kxyzwqrsmtlo`. An amendment never changes the change id, so the branch stays the same. A branch is 1 to 200 bytes of letters, digits, `.`, `_`, `/` and `-`, not starting with `-`, `.` or `/`, not ending with `/`, `.` or `.lock`, without `//`, `..` or `@{`, and not `HEAD` (`INVALID_BRANCH`).
- **The remote** is `--git-remote NAME`, or the only configured remote, or `origin`. With several and no `origin`, name one (`GIT_REMOTE_REQUIRED`, which lists them). It must be a remote the repository configures (`git remote`): a URL is never accepted (`INVALID_GIT_REMOTE`, `GIT_REMOTE_NOT_FOUND`). A remote added while the daemon runs counts at the next publish.
- **Never trunk.** The branch of `trunk()`'s bookmark, the remote's HEAD branch, a branch whose remote commit is immutable, and the branch checked out in the repository's own git working tree are refused (`BRANCH_NOT_PUBLISHABLE`, with `why`). Landing on `main` is `reef land`, not a review branch. `trunk()` is the first of `main`, origin's HEAD branch, `master` and `trunk` that exists, locally or on `origin` ([ADR 0022](adr/0022-shared-dependency-stores.md) decision 8), and the remote's HEAD is what `refs/remotes/<remote>/HEAD` says, which a `git clone` sets and a `git remote add` does not (`git remote set-head <remote> --auto` sets it). A default branch with another name, or a release branch, is protected only by the host: a publish that fast-forwards it with `--branch` is allowed. Use the host's branch protection for them.

## What is pushed, and what is refused

The change's commit and every ancestor no branch of the remote has: what jj's CLI checks before a push. Each of those commits is checked, newest first, and the first that fails refuses the publish with its change and commit in the error's metadata:

| Refusal | Why | What to do |
| --- | --- | --- |
| `CHANGE_EMPTY` | the change changes nothing | edit files, or publish its parent (`next`: `reef publish @-`) |
| `CHANGE_UNDESCRIBED` | a commit has no description | `reef publish -m <summary>`, or `reef describe <ancestor> -m <summary>` |
| `CHANGE_CONFLICTED` | a commit has conflicts | resolve them (`reef conflict show`) |
| `CHANGE_DIVERGENT` | a change has several visible commits | abandon all but one (`next` lists them) |
| `COMMIT_IDENTITY_MISSING` | an author or committer lacks a name or email | git hosts refuse such commits |
| `COMMIT_AUTHOR_DEFAULT` | a commit is authored as `reef <reef@host>` | `reef describe <change> --reset-author` |
| `INCLUDES_WORKING_COPY` | a commit is another active workspace's working copy, which that agent may still be writing | publish from that workspace (`next` names it) |
| `RANGE_TOO_LARGE` | more than 1,000 commits | the remote has almost none of the history: push trunk to it once with git |
| `IMMUTABLE_REVISION`, `HIDDEN_REVISION` | the change is trunk's, or gone | |

Your own workspace's working copy is snapshotted first and may be published. Not checked: jj CLI's `git.private-commits` and `git.sign-on-push`, which Reef's settings do not have.

## Updates, the lease and `--force`

Reef always pushes with a lease: the push succeeds only if the branch on the remote is still where Reef last saw it, or still absent if Reef never saw it. How the branch may move:

| The branch on the remote | Update | Allowed |
| --- | --- | --- |
| absent | `created` | yes |
| already this commit | `up-to-date` | yes; nothing changes |
| an ancestor of the commit | `fast-forward` | yes |
| a commit of a change being published (an earlier version of this work) | `rewritten` | yes |
| any other commit | `forced` | only with `--force`; else `BRANCH_DIVERGED` |
| several commits (a conflicted bookmark) | | `BRANCH_CONFLICTED`: fetch it, or set it with `jj bookmark set` |

"A change being published" means the change of the published commit or of one of the commits pushed with it, compared by change id, which jj writes into every commit it makes (a `change-id` header). A commit someone made or amended with plain git has none that matches, so it reads as someone else's.

**Someone else's branch takes three steps**, and that is the whole protection:

1. A branch Reef has not fetched, or that moved or was deleted since, is `BRANCH_MOVED`: fetch it (`jj -R <root> git fetch --remote <remote> --branch <branch>`, or `git -C <root> fetch <remote> <branch>`), then publish again. A branch the host deleted after a merge (GitHub's "delete branch on merge") needs jj's fetch, which forgets it; git's fetch of a branch that no longer exists fails.
2. If it holds another change's commit, that is `BRANCH_DIVERGED`, whose hint and metadata name the commit's author and summary. Publish to another branch, rebase onto it, or:
3. `--force` replaces it. A forced publish is recorded as `forced`, and its operation's description ends ` (forced)`.

`--force` never lifts the lease, and never makes trunk's branch, the remote's HEAD, an immutable branch or a checked-out branch a target. It also lets a publish move a local bookmark of the branch's name that points elsewhere, which is otherwise `BRANCH_DIVERGED` too. Where it is not needed it changes nothing.

A remote that refuses the push (a protected branch, a declined hook, no permission to the branch) is `REMOTE_REJECTED`, with the remote's reason in `remote_reason`.

## Credentials and failures

The push runs in the daemon's process, with your git configuration (credential helpers, `core.sshCommand`, `url.*.insteadOf`, `pushurl`) and the daemon's environment (`SSH_AUTH_SOCK`, `HOME`, `PATH`). Reef holds no git or host credential and reads none. Nothing prompts: every push sets `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never` and `SSH_ASKPASS_REQUIRE=never`, adds `-o BatchMode=yes` and keep-alives to an OpenSSH command (not to `plink` or a wrapper, and not when `GIT_SSH` is set), limits a stalled HTTPS push (`GIT_HTTP_LOW_SPEED_LIMIT=1000`, `GIT_HTTP_LOW_SPEED_TIME=60`), and turns git's tracing off so no credential reaches its stderr.

| Failure | Kind | Means |
| --- | --- | --- |
| `GIT_AUTH_FAILED` | denied | the remote refused the credentials, or git found none; the hint says whether the daemon has `SSH_AUTH_SOCK` |
| `GIT_UNREACHABLE` | unavailable | the host could not be resolved or reached |
| `GIT_NOT_FOUND` | unavailable | the daemon could not run git; set `[gc] git_executable` |
| `GIT_TOO_OLD` | precondition | git is older than jj needs (2.42) |
| `GIT_PUSH_FAILED` | unavailable | anything else |
| `PUBLISH_TIMEOUT` | unavailable | the push outlived `[publish] timeout`; it is still recorded if git succeeds |
| `PUBLISH_IN_PROGRESS` | unavailable | a publish of the same branch is still pushing |

Each carries git's stderr in `git_stderr`, the last 2,048 bytes, with the user information of any URL replaced by `***` and control characters removed.

`BRANCH_MOVED`'s `next` is `reef fetch --git-remote <remote> --branch <branch>` ([rebasing](rebasing.md#the-fetch)), after which a publish sees what the branch holds; `BRANCH_DIVERGED`'s hint names `reef rebase --onto <branch>@<remote>` to build on it. After a host's squash merge of a published change, `reef rebase` reports the change empty: its edits are on trunk.

**A daemon started from another shell has that shell's environment.** If `git push` works in your terminal and a publish says `GIT_AUTH_FAILED`, the daemon probably lacks your SSH agent: `reef daemon stop`, then any `reef` command from your terminal starts it again with that terminal's environment.

## The pull-request command

From the remote's push URL, Reef derives the host and the repository's path and returns how to open a pull request:

| Host | Command (`next`) | Page |
| --- | --- | --- |
| `github.com`, or a host in `[publish] github_hosts` | `gh pr create --repo … --head … [--base …] --title … --body …` | the compare page |
| `gitlab.com`, or a host in `[publish] gitlab_hosts` | `glab mr create --repo … --source-branch … [--target-branch …] --title … --description … --yes` | the new merge request page |
| `bitbucket.org` | none | the new pull request page |
| `codeberg.org` | none | the compare page |
| anything else | none | none: the remote's own messages are printed, which GitHub and GitLab fill with a link |

The base is trunk's bookmark when the remote has it, else the remote's HEAD branch. The title is the description's first line and the body what follows the first blank line. The command is returned as an argument list (`pull_request.argv`), and the CLI and MCP print it shell-quoted, so a description cannot inject shell syntax. Both `--repo` and `--head` are given because a Reef workspace has no `.git` for `gh` to read.

The remote's messages are returned as the remote's (`remote_messages`): control characters removed, at most 20 lines of 300 bytes, and never in `next`.

## Trailers and `--signoff`

A description is stored byte for byte, so `Signed-off-by:` and `Co-Authored-By:` trailers survive describe, publish and the pushed commit. `--signoff` (on `reef publish` and `reef describe`) adds `Signed-off-by: <name> <email>` for the daemon's `[identity]`, as `git commit -s` does, unless that exact trailer is already there; without `-m` it keeps the description. A sign-off certifies the [Developer Certificate of Origin](https://developercertificate.org/) in that person's name, so Reef adds one only when asked, and never with the default identity.

## Hooks and checks

No git hook runs: jj pushes with `--no-verify`, and a Reef workspace has no `.git`. Run the repository's checks before publishing, with `reef job submit --wait` for those in `reef-checks.toml`, or the project's own command inside `reef ws run`. A gate that refuses to publish what policy or checks would block is a follow-up.

## Bookmarks in jj

The local bookmark follows later rewrites of the change, so `reef publish` (or `jj git push`) sends the amended commit to the same branch. In a colocated repository the jj CLI writes the local bookmark to git on its next command. To delete a published branch, run `jj bookmark delete <branch>` and `jj git push --deleted` from the repository's root, or delete it on the host (or with `git push <remote> --delete <branch>`), then `jj git fetch` and `jj bookmark delete <branch>`. A publish cannot be undone with `reef undo`: undoing the bookmarks would not unpush it (`OPERATION_NOT_UNDOABLE`).

## Snapshots that left the machine

Every snapshot row starts local: only a change that is explicitly published leaves the machine ([design](design.md)). A publish marks as published the rows whose content it pushed, and reports how many in `local_snapshots_published`: the rows of the pushed commits, and the rows of a published change whose tree is the pushed commit's. The second rule covers `reef publish -m` and `--signoff`, whose describe writes the pushed commit over the last snapshot without changing its files. Earlier snapshots with other files stay local: git never sent them. After GC rewrites the operation log, the reindex applies the same rule against every remote bookmark, so commits the jj CLI pushed read as published too.

## Who may publish

Any client on the daemon's socket: one user is one trust domain ([ADR 0027](adr/0027-trust-boundaries-in-phase-1.md)), and that client could run `git push` itself. Nobody through the remote listener (`REMOTE_PUBLISH_DENIED`): a remote token must not become the user's git credentials, and a push can start the host's CI with its secrets. Publish from the daemon's machine.

An operator can limit it:

| `[publish]` | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | `false` refuses every publish (`PUBLISH_DISABLED`) |
| `remotes` | `[]` | the remotes a publish may push to; empty allows every configured one (`GIT_REMOTE_NOT_ALLOWED`) |
| `branch_prefix` | `"reef/"` | the default branch's prefix; empty for none |
| `timeout` | `"5m"` | how long a request waits for the push, from 10 s to 1 h |
| `github_hosts`, `gitlab_hosts` | `[]` | self-hosted hosts treated as GitHub or GitLab for the command |

A harness sandbox that denies an agent the network but lets it reach Reef's socket or `reef mcp` lets it push its change to a configured remote with your credentials: deny the socket, or set `enabled = false` or `remotes` ([threat model](threat-model.md#harness-sandboxes)).

## Not yet

- A policy or checks gate before a publish.
- Stacked pull requests: each change of a stack on its own branch, based on its parent's.
- Publishing through the remote listener, and forwarding a remote client's SSH agent.
- A native gitoxide push (R4.14) and the landing service's push of landed history (R3.17), which reuse this plan, lease and record.
