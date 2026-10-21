# 0024. Repository onboarding: implicit registration and jj alongside git

- Status: accepted
- Date: 2026-10-21
- Ticket: R1.25

## Context

Before this decision, a repository had to be a jj repository and registered before any workspace command worked: `jj git init --colocate`, then `reef repo register`, then `reef ws run`. Without those steps, `reef ws run` in a plain git checkout answered `NotFound: repository containing /path`. A human following the quickstart can do two setup steps. An agent dropped into a git checkout and told to use Reef cannot tell from that error that the fix is to install a second tool, learn it, and initialize a `.jj` directory. It stops there.

Both steps are mechanical, and Reef can do them itself: jj-lib already creates the jj side of a colocated repository, and registration is one RPC. The question is who does them, when, and what the user's checkout gains.

## Decision

We will register a repository the first time a command needs it, and create jj alongside git when the repository has only git.

1. **Registration can initialize jj alongside git.** `RegisterRequest.if_not_jj` is `FAIL` (the old behavior) or `INIT_ALONGSIDE_GIT`. With the second, when no jj repository contains the path, the daemon walks up to the nearest directory with a `.jj` or a `.git`. If `.git` is a directory, it adds `/.jj/` to `.git/info/exclude` and calls `Repo::init_in_git_repo`, which wraps jj-lib's `Workspace::init_external_git` on `<root>/.git` and then registers as before. `RegisterResponse.initialized_jj` says that this happened. A `.git` file (a git worktree or submodule checkout) is `InvalidArgument`: run from the main checkout. A path with no `.git` is still `NotFound`, now with "run git init first". Reef never creates a repository from nothing.
2. **The CLI registers, not the daemon.** When a command that resolves `--repo` (or the current directory) by path gets `NotFound` for `repository containing`, the CLI calls `Register` with `INIT_ALONGSIDE_GIT`, prints `reef: registered <root>` on stderr (with ` and initialized jj alongside git` when it did), and retries once. A second failure is returned as it is. A `--repo` given as an id or prefix, a path that does not exist, and `repo forget` never register anything. `reef repo register` initializes by default, and `--no-init` refuses a non-jj repository as before.
3. **Opt-outs at two levels.** `--no-register` turns implicit registration off for one command, and `[repo] auto_register = false` in `config.toml` turns it off for every command.
4. **Colocated only.** The user keeps using `git` in the checkout. The daemon imports git's refs into jj's view before it resolves a revision a client names (R1.05), so `main` and the user's branches are there for the first workspace. Nothing is exported back to git in Phase 1.
5. **What the checkout gains**: a `.jj` directory and one `/.jj/` line in `.git/info/exclude`, unless a line `/.jj/`, `.jj/`, or `.jj` is already there. `rm -rf .jj` removes jj again; as after `jj git init --colocate`, the empty working-copy commits jj-lib wrote (one on the root commit, then one on HEAD) stay in `.git` as objects under `refs/jj/keep/*` refs, which `git branch` and a plain `git log` do not show. `reef repo forget` only forgets the registration. Tracked content, including `.gitignore`, is never edited. The exclude line is written before `.jj`: if a step fails or the process dies after it, all that is left is a line ignoring a `.jj` that is not there, and the next attempt initializes.
6. **The initial commit.** jj-lib's init leaves the working-copy commit on the root commit with an empty tree. Like `jj git init --colocate`, Reef then imports git's HEAD, checks out a new empty change on it, and records HEAD's tree as the working-copy state without reading the files, so jj's next snapshot of the checkout rehashes them and finds only the user's own uncommitted edits. With an unborn HEAD (no commits yet) the working-copy commit stays on the root commit. The identity on that commit and its operation is the daemon's `[identity]`, as for every snapshot. Nothing prompts.
7. **Registrations take turns.** The daemon runs one `Register` at a time, from opening or initializing the repository to recording it. Two agents starting `reef ws run` in one fresh checkout at once therefore initialize it once: the first records it and reports `initialized_jj`, and the second finds it registered and gets `already_registered`.
8. **Visible.** The CLI gives `Register` 10 minutes unless `--timeout` says otherwise, because initializing indexes the whole history. The daemon logs `repository initialized alongside git` at `info` with `repo` and `root`. The CLI's stderr line is silenced only by `--quiet`, and it goes to stderr in JSON mode too, so `ws run -o json` never hides that the checkout was changed.

## Consequences

- An agent with only `reef` on `PATH` reaches a working `reef ws run` in any git checkout with no setup command, which RX.07 measures. The quickstart (R1.24) drops its jj prerequisite and its register step.
- Running a `reef` command in a git checkout changes that checkout. The change is small, ignored by git, reversible, and announced. A user who does not want it passes `--no-register` or sets `auto_register = false`.
- `jj log` in the user's checkout shows the import operation and an empty working-copy change on HEAD, which is what `jj git init --colocate` produces. Reef never snapshots the user's own checkout (the watcher watches Reef workspaces only), so that change stays empty until the user runs `jj` there.
- Git changes made after the init (commits, branch moves, fetches) reach jj's view through the ref import before each workspace create. A moved HEAD is not imported into the default workspace's working copy until the user runs `jj` in the checkout, as with any colocated repository Reef does not drive.
- A `jj` binary newer than jj-lib 0.46 may upgrade on-disk formats when the user runs it in a Reef-initialized repository ([0008](0008-jj-lib-embedding-findings.md)'s compatibility statement applies). The quickstart's limitations list says so.
- Tools that do not read `info/exclude` (some IDE indexers) see `.jj/`.
- `--repo` is now a path when the text names something on disk, and a name otherwise. A 16-hex-character directory that exists is taken as a path, which registers what the user pointed at.

## Alternatives considered

- **The daemon registering on any `NotFound`**, with no CLI involvement. Rejected: the daemon would change a user's checkout as a side effect of resolving a path, and nothing in the response of the failed command would show it. The CLI turns it into one visible line and one retry.
- **Shelling out to `jj git init --colocate`.** Rejected: it needs `jj` on `PATH`, a second install for the agent to discover, and jj-lib already does the same thing in-process.
- **An internal store, `.jj` only, without colocation.** Rejected: the user's `git` would stop seeing jj's commits, and R1.05's ref import, which lets a branch made with git resolve as a bookmark, is built for colocated repositories.
- **Ignoring `.jj/` in `.gitignore`.** Rejected: it edits tracked content, which shows up in the user's next commit. `info/exclude` is local to the clone, and it is where the `jj` CLI writes the same line.
