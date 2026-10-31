# Quickstart

## What you get

Reef gives each coding agent its own workspace: a copy-on-write copy of your git or jj repository, leased to one process and snapshotted after every write. When the agent exits or dies, its last edits land as a jj change and the directory is reclaimed. Nothing is lost, and no worktree is left behind.

This page takes about ten minutes. The example repository is [`pillarjs/path-to-regexp`](https://github.com/pillarjs/path-to-regexp); any git or jj repository works. Output is from macOS, trimmed to the lines that matter (`...` marks the rest); the same steps were run on Linux with kernel overlayfs and with fuse-overlayfs, and the page says where Linux differs.

## Install

If you already run 0.1, read [Upgrading from 0.1](../CHANGELOG.md#upgrading-from-01) first.

```sh
curl -fsSL https://raw.githubusercontent.com/reef-vcs/reef/main/install.sh | sh
reef --version
```

```text
reef install: downloading reef 0.2.0 for aarch64-apple-darwin
reef install: installed reef 0.2.0 (21887d1cd101, aarch64-apple-darwin) in /Users/you/.local/bin
reef install: /Users/you/.local/bin is not on PATH; add this line to your shell's profile:
  export PATH="$HOME/.local/bin:$PATH"
reef 0.2.0 (21887d1cd101, aarch64-apple-darwin)
```

The installer checks the tarball against the release's `SHA256SUMS`, never prompts, and never uses `sudo`. `REEF_VERSION=0.2.0` pins a release and `REEF_INSTALL_DIR` installs elsewhere. Or install with `brew install reef-vcs/tap/reef`, or `cargo binstall --git https://github.com/reef-vcs/reef reef-vcs reef-daemon`, or build `main` from source with `cargo install --locked --git https://github.com/reef-vcs/reef reef-vcs reef-daemon` (Rust 1.97.1 or later). Install both: `reef` runs the daemon from `reef-daemon`, beside it or on `PATH`. The tarballs and the formula carry shell completions; `reef completions zsh` prints them too. The binaries are not signed yet. From 0.1.0, the release workflow attests each tarball's build provenance: with a logged-in `gh`, `gh attestation verify reef-<version>-<target>.tar.gz --repo reef-vcs/reef` checks that a tarball was built by this repository's release workflow. The Linux binaries are static and run in any container.

Reef needs macOS 13 or later on APFS, or Linux with kernel overlayfs (a daemon with `CAP_SYS_ADMIN`, typically root in a VM or container) or `fuse-overlayfs` on `PATH`, or btrfs or XFS for reflinks. A root daemon makes every client of its socket root and runs store builds as root, so use the kernel tier in a VM or container dedicated to the agents, and give it a state directory of its own (`REEF_STATE_DIR=/var/lib/reef`, say): it refuses one another user owns, such as yours under `sudo -E`. Anywhere else it copies, which works but is slow on large trees. In a container, put the state directory on a volume: kernel overlayfs cannot stack on the container's own overlay filesystem, so without one Reef uses fuse-overlayfs. Reef doesn't need `jj`: `reef status`, `reef log`, `reef diff` and `reef undo` show and revert what agents did. If you use [jj 0.46](https://github.com/jj-vcs/jj/releases/tag/v0.46.0), the version Reef is built on, it works in your checkout alongside Reef.

## Start the daemon

Any `reef` command starts the daemon if it is not running. To start it yourself:

```sh
reef daemon start
reef daemon status
```

```text
reef: started daemon (pid 5691)
socket:            /Users/you/Library/Application Support/reef/daemon.sock
state_dir:         /Users/you/Library/Application Support/reef
...
backends:          overlayfs (unavailable: overlayfs is Linux only), ..., apfs (apfs), ..., copy (apfs)
```

The state directory holds the daemon's database, logs, and workspaces. On Linux it is `~/.local/state/reef`, with the socket under `$XDG_RUNTIME_DIR` when that is set. `reef daemon stop` stops it.

## No setup

There is no setup step. In a git checkout, the first `reef` command registers the repository and initializes jj alongside git:

```sh
git clone https://github.com/pillarjs/path-to-regexp.git ~/src/path-to-regexp
cd ~/src/path-to-regexp
reef ws run -- true
reef repo list
```

```text
reef: registered /Users/you/src/path-to-regexp and initialized jj alongside git
ID                ROOT                             COLOCATED  REGISTERED
27454214f4ebdade  /Users/you/src/path-to-regexp    true       2026-10-23T20:02:27.803Z
```

That added a `.jj/` directory and one line, `/.jj/`, to `.git/info/exclude`, so `git status` stays clean. `git` keeps working as before. To undo it: `reef repo forget .` and `rm -rf .jj`. `reef repo register` does the same registration explicitly.

Reef starts each new workspace on your default branch: `main`, else the branch `origin/HEAD` names (`git clone` sets it), else `master` or `trunk`, else the same on `origin`. Here that is `master`. In a repository with none of these, a workspace starts on top of your checkout's own uncommitted state, and `reef ws create` and `reef ws run` say so; pass `--new-on <branch>` there.

## Run an agent in a workspace

`reef ws run` creates a workspace, runs a command in it, keeps the lease alive while the command runs, and releases the workspace when it exits. Reef does not start or configure the agent; it runs whatever you give it.

```sh
reef ws run --purpose "add a usage note" -- claude --permission-mode acceptEdits \
  -p "Create NOTES.md with two short lines: what this library does, and the one function most callers use."
```

From another terminal, while it runs, `reef ws list`:

```text
ID                STATE   CHANGE    OWNER                                  LEASE     BACKEND  ATTENTION  PATH
28a325ae9b734048  active  pursxlpq  you@laptop:16849                       29m left  apfs     -          ~/Library/.../ws/28a325ae9b734048
```

The agent works in that path. Inside a workspace, `REEF_WORKSPACE` names it, and `reef here` prints the workspace, its change, its lease, and what to do next. When the agent exits, its edits are a jj change. Look at it from your checkout:

```sh
reef diff -r pursxlpq
```

```text
diff --git a/NOTES.md b/NOTES.md
...
+++ b/NOTES.md
@@ -0,0 +1,2 @@
+path-to-regexp turns Express-style path strings like `/user/:name` into regular expressions for route matching.
+Most callers use `match(path)`, which returns a function that tests a URL path and yields its named params.
```

A workspace whose agent changed nothing leaves no change behind.

## Look at what changed

These work from your checkout, or inside a workspace, where `@` is the workspace's change and each command snapshots it first:

```sh
reef status
reef log --limit 10
reef show pursxlpq
reef diff --symbols -r pursxlpq
reef op log --limit 5
reef history src --since 7d
reef status --at 1h
```

`reef status` lists a workspace's changed files, or the repository's state outside one; `reef log` lists the changes in view, newest first; `reef show` prints one change and the paths it changes; `reef diff --symbols` lists the functions, classes and other definitions a change added, removed, changed or moved, with where the changed ones are called, and `reef diff --expand <name>` shows one of them line by line; `reef op log` lists what agents did, newest first, with the agent that did it, `reef history <path>` (or `--symbol <name>`) lists the changes that touched it, who wrote them and whether they landed, and `reef op watch` prints operations as they are recorded (`--kind describe --count 1` waits for one describe). `--at 1h` (or `--at-op <op>`) reads as of then, from the operation log. `reef undo <op>` reverts one operation, and `reef describe -m <text>` sets a change's description. Revisions are jj revsets: a change id prefix, a bookmark, `trunk()`. Each command prints brief JSON when piped (`-o json` for every field), and `reef file read -r <rev> <path>` prints a file at any revision without checking it out.

## Set up your agent

So that an agent starts in a workspace without being told, write Reef's instructions into the repository:

```sh
reef init
reef init --user
```

```text
$ reef init
wrote AGENTS.md
$ reef init --user
wrote CLAUDE.md
wrote .claude/skills/reef/SKILL.md
wrote /Users/you/.claude/skills/reef/SKILL.md
```

`reef init` writes `AGENTS.md`, and `CLAUDE.md` and a Claude Code skill when the repository uses Claude Code or Claude Code runs `reef init`. `--user` also writes the skill for every repository on this machine, and turns on the Claude files here. `reef init --check` exits 3 when a file is stale, for CI. `reef guide` prints the same text to paste into any other agent's instructions. Aider and Gemini CLI read `AGENTS.md` only when their configuration names it.

With Claude Code, `reef init` also writes `.mcp.json`, which starts `reef mcp`: Reef's API as MCP tools ([ADR 0035](adr/0035-mcp-server.md)). Claude Code asks before it starts a project's server. The tools default to the workspace or repository Claude Code was started in. An agent without a shell reads and edits files through them (`read_file`, `edit_files`), and `wait_for_operations` waits for another agent's describe or edit. For another harness, add `reef mcp` as a stdio server in its MCP configuration.

## Watch it reclaim

A workspace whose agent stops heartbeating is reclaimed: a final snapshot, then the directory goes. Make one with a one-minute lease, write a file, and walk away:

```sh
cd "$(reef ws create --lease 1m --purpose 'reclaim demo')"
echo "notes from an agent that went away" > NOTES.md
cd ~/src/path-to-regexp
```

The lease has a five-minute grace period (`lease.grace`) after it ends. About six minutes later:

```sh
reef ws list --all
reef file read -r tmvnprqw NOTES.md
```

```text
ID                STATE      CHANGE    OWNER       LEASE             BACKEND  ...
4de211219c73788b  reclaimed  tmvnprqw  you@laptop  ended (expired)   apfs     ...
notes from an agent that went away
```

The same happens to an agent killed with `kill -9` in the middle of `reef ws run`: nothing heartbeats its lease, and what it wrote is in the final snapshot.

## Two agents on one change

Two workspaces can edit the same change: `reef ws create --change <id>`. Once both have written, the change is divergent: two versions of it, both kept.

```sh
reef ws create --change ztxnkruw --purpose "agent B"
reef ws list
```

```text
ID                STATE   CHANGE    ...  ATTENTION  ...
36ec39108499b805  active  ztxnkruw  ...  divergent  ...
be530c94842cd504  active  ztxnkruw  ...  -          ...
```

Nothing is lost, and `reef here` in the divergent workspace names the command that lists both versions:

```sh
reef log -r 'change_id(ztxnkruw)'
```

Abandon the one to drop with `reef abandon <commit id>`; the workspace on it moves to the other.

## Shared dependency stores

Twenty workspaces should not mean twenty `node_modules`. Declare a store in `reef.toml` and commit it; Reef reads `reef.toml` from the commit a workspace starts on, not from disk.

```sh
cat > reef.toml <<'EOF'
[[store]]
name = "node_modules"
lockfile = "package-lock.json"
mount = "node_modules"
build = ["npm", "ci", "--ignore-scripts"]
EOF
git add reef.toml && git commit -m "Share node_modules between agent workspaces"
reef store build node_modules
reef store list
reef ws run -- npm run build
```

```text
added 222 packages, and audited 223 packages in 2s
built store node_modules 9571f160d298 at ~/Library/.../stores/node_modules/9571f160d298.../node_modules
NAME          LOCKFILE      SIZE     IN_USE  BUILT                     PATH
node_modules  9571f160d298  151 MiB  0       2026-10-23T20:09:01.140Z  ~/Library/.../node_modules
> path-to-regexp@9.0.0 build
> Running "tsc --build"...
```

The daemon runs the build once per lockfile, seals the result read-only, and mounts it into every workspace whose lockfile matches. A workspace whose lockfile has no build fails to create, naming the `reef store build` to run. The mount is a read-only bind mount when the workspace is on kernel overlayfs (Linux as root), and a symlink otherwise (macOS, fuse-overlayfs).

## Move a workspace

A workspace can move to another change and keep its lease, path, and stores: `reef ws move <id> --new-on master`, or `--change <id>` to edit an existing one.

## Backup and restore

```sh
reef backup ~/reef-backup
reef daemon stop
reef restore --force ~/reef-backup
reef daemon start
```

```text
backed up the database and 1 repository (159 files, 1.8 MiB) to /Users/you/reef-backup in 0.0s
stopped daemon (pid 5691)
restored the database and 1 repository from /Users/you/reef-backup; 2 active workspaces kept, rematerialized when the daemon starts
reef: started daemon (pid 14602)
```

A backup is the daemon's database and each repository's jj store, taken while agents keep writing. On a new machine, `reef restore` needs no `--force`; on this one, `--force` replaces the current state and goes back in time. After a restore, leases get a grace period before anything is reclaimed.

## Reach the daemon from another machine

A daemon in a cloud VM, or on your desktop, can serve the CLI and the web UI to another machine over TLS with a token ([ADR 0049](adr/0049-remote-listener.md)). On the daemon's machine, start it with the remote listener and issue a token:

```sh
reef daemon start --serve-remote     # or [remote] enabled = true in config.toml
reef remote token create laptop --scope write
```

`token create` prints the token once, the key's fingerprint, and the line to run on the other machine, with the token on stdin so it never sits in your shell history:

```sh
reef remote add vm https://vm.example:7470 --fingerprint sha256:9f2c… --token-file - < token.txt
reef --remote vm ws list --repo <id>
reef --remote vm status --workspace <id>
```

Compare the fingerprint with `reef remote show` on the daemon's machine, over a channel you trust: that is what tells your client the daemon is the real one. The listener speaks only TLS; by default the daemon generates its own certificate and clients pin its key. `[remote] tls_cert` and `tls_key` use a certificate from a CA instead (a domain with an ACME client, or your VPN's certificate service), which needs no pin and is what a phone's browser needs. `[remote] listen` is `0.0.0.0:7470` by default and `[remote] names` the host name.

- A token is `read` by default and expires in 30 days: `--scope write` lets it describe, abandon, undo, edit and create workspaces; `--expires never` keeps it until `reef remote token revoke`. `remote token list` shows when each was last used and from where; `rotate --overlap 1d` gives a new secret while the old one still works.
- In remote mode a command needs `--workspace` or `--repo`: this machine's directory is not on the daemon's. `here`, `ws run`, `ui`, `init`, `mcp`, `daemon` (except `status`), `config`, `backup`, `ws gc`, `store build`, `repo register` and `remote token` run only on the daemon's machine.
- The web UI is at the URL `token create` prints (the link holds the token). With the generated certificate, the browser warns once; compare the fingerprint in its details before accepting.
- Until Reef has had an external security review, serve the listener on a private network (a VPN or a cloud VPC), not the open internet. A `read` token sees presence and workspace purposes, which carry agents' command lines.

## Configuration

`reef config show` prints the effective configuration and where each value came from. The settings you are most likely to change, in `config.toml` in the state directory:

```toml
[lease]
default = "30m"      # how long a workspace lives without a heartbeat
[workspace]
backend = "auto"     # apfs, overlayfs, fuse-overlayfs, reflink, or copy
[watcher]
backend = "auto"     # Watchman for repositories over 50,000 files when it is installed
[gc]
undo_window = "7d"   # snapshots and operations kept at least this long
interval = "1d"      # how often the daemon collects garbage by itself; "0s" turns it off
```

The daemon collects garbage daily: old snapshots, unused bases, and unused stores. `reef ws gc` runs it now, and `--dry-run` reports what it would free.

## Known limitations in Phase 1

- Watcher snapshots are local: they stay in the repository on this machine and are never pushed or exported to git.
- New workspaces start on `main`, `origin/HEAD`'s branch, `master` or `trunk` ([ADR 0022](adr/0022-shared-dependency-stores.md) decision 8 has the order). In a repository with none of these they start on the checkout's own working-copy change, and a `jj` command in the checkout rebases them; pass `--new-on <branch>`.
- In a git checkout, Reef imports branches but does not export: an agent's change is a jj change, not a git branch, until you make it one ([ADR 0024](adr/0024-repository-onboarding.md)).
- A workspace directory has neither `.git` nor `.jj`, so `git` and `jj` commands inside it fail: agents work on files, and you run `git` and `jj` in your checkout ([ADR 0014](adr/0014-workspace-model-on-jj.md)).
- `reef.toml` is read from the commit a workspace starts on, not from disk: commit it before it takes effect.
- A shared store is read-only, so a tool that writes a cache into it fails (Vitest's `node_modules/.vitest-cache`; Vite's `cacheDir` setting moves it).
- On macOS, and on Linux without `CAP_SYS_ADMIN`, a store is protected by file permissions only: a process running as the same user could make it writable again ([ADR 0022](adr/0022-shared-dependency-stores.md), [ADR 0016](adr/0016-overlayfs-privilege-tiers.md)).
- A workspace is not a sandbox. An agent runs as you and can do anything you can, in every workspace and through the daemon's socket; Reef isolates accidents and hostile repository content, not a hostile agent ([threat model](threat-model.md)).
- If your agents run in a harness sandbox, the sandbox does not know about Reef's socket: an agent that can reach it can create a workspace or a backup at any path you can write, and run a store build, outside the sandbox. Deny the socket (`<state>/daemon.sock`, and on Linux `$XDG_RUNTIME_DIR/reef/daemon.sock`) to keep it out, or set `[store] allow_builds = false` to let it use workspaces but never build ([threat model](threat-model.md#harness-sandboxes)).
- `reef store build` runs the command the repository's `reef.toml` declares, as your user, with `[store.env]` and network access. It prints the command first; `[store] allow_builds = false` in the daemon's `config.toml` turns builds off.
- Reef registers only repositories you own (their `.jj` or `.git`), as git's `safe.directory` does; `[repo] allow_foreign_owner = true` lifts that, for a daemon serving another user's checkout on purpose.
- Reef is built on jj-lib 0.46. A newer `jj` may upgrade the repository's on-disk formats when you run it there ([ADR 0008](adr/0008-jj-lib-embedding-findings.md)).
- Tools that do not read `.git/info/exclude` see the `.jj/` directory.
- The remote listener's generated certificate is self-signed: browsers warn once per origin, and a phone's browser needs a supplied certificate. A restored state directory has a new key, so clients re-add the remote with its new fingerprint, and a backup older than a revocation brings the revoked token back: revoke it again after a restore.
- The binaries are not signed or notarized. Windows is not supported.

## Where next

- [The design](design.md): why Reef exists and where it is going.
- [Phases and tickets](dev-phases.md), the [architecture decisions](adr/README.md), [benchmarks](benchmarks.md), and [load tests](load-tests.md).
- Report a problem with the [issue template](https://github.com/reef-vcs/reef/issues/new/choose).
