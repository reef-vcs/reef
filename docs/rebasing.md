# Rebasing and fetching

`reef rebase` brings a workspace's change onto the newest trunk: it fetches trunk's branch from the repository's git remote, moves the change (with every change below it that trunk lacks, and every change above it) onto trunk's head, and updates the files in every workspace it moved. No jj is needed at the root, and nothing runs in the root checkout. [ADR 0071](adr/0071-rebasing-and-following-trunk.md) records the decisions; this page is how to use it.

```sh
reef rebase                         # from inside a workspace: fetch, then rebase onto trunk
reef rebase --onto main@origin      # a destination of your choice (also -d, --destination)
reef rebase --source                # only this change and those above it
reef rebase --no-fetch              # onto the trunk Reef last saw (it snapshots first, always)
reef fetch                          # fetch trunk's branch alone; --branch, --git-remote
```

The MCP server's `rebase_change` tool and the SDKs' `workspace.rebase()` do the same.

## What moves

| Mode | Moves | Like |
| --- | --- | --- |
| branch (default) | every commit of the change's ancestry the destination lacks, from its roots, with all their descendants | `git rebase main` on a branch, `jj rebase -b @ -d main` |
| `--source` (`-s`) | the change's commit and its descendants | `jj rebase -s @ -d main` |

Change ids stay; commit ids change. Every workspace on a moved commit is snapshotted first, and afterwards checks out its new tree; edits it made in between are carried onto the new commit ([ADR 0039](adr/0039-carrying-edits-through-a-write.md)). Bookmarks on moved commits move with them.

**What it costs on disk.** Each moved workspace's directory is updated by jj's tree-diff check-out: only files that differ between its old and new trees are written or removed. A rebase over a trunk that changed 100 files of 4 KiB grows a workspace by about 400 KiB, on any backend ([benchmarks](benchmarks.md#rebasing-259)).

## The destination

Without `--onto`, the destination is `trunk()` after the fetch, or its remote-tracking bookmark when that is ahead of a local one: a git user's local `main` is rarely fast-forwarded, and Reef never moves it to get there. The answer's `onto` says which (`main`, or `main@origin` with a note that the local bookmark was left behind).

## The fetch

By default a rebase first fetches the destination's branch (trunk's, or the bookmark `--onto` names; a commit or a revset fetches nothing) in an operation of its own, of kind `fetch`. The daemon runs the user's git with the daemon's environment, every prompt disabled, as a publish does ([publishing](publishing.md#credentials)), never on the repository's thread, and within `[fetch] timeout` (default 2 minutes). It fetches named branches only: no tags, no other branches.

- The remote-tracking bookmark (`main@origin`) moves to what the remote has.
- A tracked local bookmark of the same name is fast-forwarded, **except git's checked-out branch** in a colocated repository: a person's checkout is never moved under them; the note says so, and the rebase targets the remote bookmark.
- A landing destination whose policy pushes it (`[landing] push`, R3.17) is only ever fast-forwarded by a fetch, tracked or not: when it and the remote have diverged, or the remote changes a policy file, it is left where it is and a note says so, rather than merged into a conflict that would stop its landings ([landing](landing.md#pushing-landed-history)).
- A branch the remote does not have is reported `missing` and left as it is. Run `jj git fetch` to forget a branch deleted on the host.
- A fetch is not undoable: it records what the remote has. Fetch again instead.

**A failed fetch fails the rebase**, with its reason (`GIT_AUTH_FAILED`, `GIT_UNREACHABLE`, `GIT_FETCH_FAILED`, `FETCH_TIMEOUT`, `FETCH_IN_PROGRESS`, `FETCH_DISABLED`) and `next` the same command with `--no-fetch`, which rebases onto the trunk Reef last saw. A repository with no git remote rebases without fetching and says so. `[fetch] enabled = false` in the daemon's config refuses every fetch.

Through the remote listener a rebase never fetches (the note says so), and `reef fetch` is refused: a remote token never reaches the user's git credentials.

## Conflicts

A rebase whose result conflicts is written and succeeds (exit 0). The conflicted files carry jj's markers on disk, and the answer lists each change that introduces a conflict, with its first paths, and `next` is `reef conflict show -r <change>`. The conflict is assigned to the session that owns the change ([conflicts](conflicts.md)). Resolve the files; the next snapshot records the resolution. `reef undo <operation>` returns everything to before the rebase.

## Proposals

A rebase records no lifecycle transition. A proposed change it moves reads `amended`: the reviewers' view and the evidence were for the old tree, so propose it again (`reef propose`, which is the answer's `next`) once the rebased tree is ready. A queued change is refused (`CHANGE_QUEUED`): the landing service lands exactly the queued commit and rebases it itself.

## Refusals

| Reason | Why | What to do |
| --- | --- | --- |
| `INCLUDES_WORKING_COPY` | another workspace's change is below this one, and branch mode would move it under that agent | `next`: rebase that workspace, which moves both; or `--source` |
| `IMMUTABLE_REVISION` | a commit to move is already on trunk or under a tag | rebase a change above it; start new work with `reef ws move <id> --new-on main` |
| `CHANGE_DIVERGENT` | a change to move has two visible commits | abandon the one you do not want |
| `CHANGE_QUEUED` | a change to move is queued to land | wait for it, or withdraw it first |
| `REBASE_ONTO_DESCENDANT` | the destination is one of the moved commits or above one | choose a destination outside the stack |
| `RANGE_TOO_LARGE` | more than 1,000 commits would move | `--source`, or a smaller set |
| `NO_DESTINATION` | no `--onto` and no `trunk()` | name the destination |

Already on the destination's head, a rebase records nothing (`up_to_date`) and exits 0. A change whose edits the destination already has (after a host's squash merge) is kept, reported empty, with a note: abandon it with `reef abandon`, or keep working in it.

## A workspace whose change has landed

When a workspace's change reaches `trunk()`, or `main@origin` when that is ahead of your local `main` (landed by Reef, merged on the host and fetched, or a bookmark moved by hand), the daemon gives the workspace a new empty change on top of it, so its next edit is a new change rather than an amendment of what landed ([#248](https://github.com/reef-vcs/reef/issues/248)). For a proposed change this happens in the operation that records the landing. Until the daemon acts, and for a commit under a tag, a snapshot never rewrites a landed commit: it starts the new change itself, and its operation's description says so. `reef rebase` on a workspace whose change has landed starts the new change on the destination.

## `reef here`

`reef here` shows how far the workspace's change is behind trunk (`behind_trunk`, as of Reef's last fetch or ref import) and suggests `reef rebase` when it is.
