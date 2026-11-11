# 0072. Default workspace root outside the state directory

- Status: accepted
- Date: 2026-11-11
- Ticket: [#260](https://github.com/reef-vcs/reef/issues/260) ([brief](../dev-tickets/phase-3/issue-260-macos-workspace-path.md))
- Amends: [0013](0013-daemon-state-directory-and-configuration.md)'s layout

## Context

ADR 0013 put workspaces in the state directory, at `<state>/repos/<repo-id>/ws/<ws-id>`. On macOS that path is about 89 bytes and holds a space (`~/Library/Application Support`). The space breaks every unquoted shell use of a workspace path, which agents and repositories' own scripts write often. The length leaves about 14 bytes under the 103-byte Unix socket path limit, so tests and tools that make a socket in a workspace fail. Linux's default has no space, but at 67 bytes plus the user name it is nearly as tight.

## Decision

1. **For the platform's default state directory, workspaces default to `~/.reef/<repo-short>/<ws-id>`**, on macOS and Linux. `<repo-short>` is a new `[workspace] root` placeholder: the repository id's first 8 hex characters. The directory name stays the full 16-hex workspace id, the name every command takes. For a 14-byte user name the path is 53 bytes, which leaves 50 under the socket limit.
2. **A state directory given explicitly** (`REEF_STATE_DIR`, `--state-dir`) keeps `<state>/repos/<repo-id>/ws`, as it keeps its socket (ADR 0013's `$XDG_RUNTIME_DIR` rule). So does a Linux default state directory outside the home directory (`XDG_STATE_HOME` on another disk), so that workspaces stay on the base cache's filesystem. A configured `root` wins either way. `reef config show` prints the resolved default, and a `config.toml` that names that same root keeps it Reef's own.
3. **Registration draws repository ids until the 8-hex prefix is unused** among registered repositories. Repositories registered earlier that share a prefix share a directory. That is harmless, because workspace ids are unique across repositories.
4. **Nothing moves.** Rows record each workspace's path, so workspaces created under the old root serve and release there. Under the `~/.reef` default, the start sweeps (trash, interrupted creates, Linux's overlay reconcile) read both the root and `<state>/repos/<repo-id>/ws`.
5. **`~/.reef` is Reef's own and private** (S-185). At every start it gets S-01's check without being created: another uid's directory refuses the start (exit 7, naming `[workspace] root`), and group or other bits are tightened to `0700` with a warning. A create, and a rebuild after a restore, make `~/.reef` and `~/.reef/<repo-short>` with mode `0700`. A configured root gets no new check. The reference executor's sandbox hides `~/.reef` from checks, as it hides the state directory.
6. **The base cache stays in the state directory.** In a default install `~` is on the same volume, and the clone probe already falls back to copying when it is not.
7. **Forgetting a repository** removes trash and an empty directory under `~/.reef`, never recursively.

## Consequences

- New workspaces have short, space-free paths. Reef's own "known workspace artifact", the socket-path config test, passes in them.
- Workspaces of the default state directory are outside it. `restore --into` another state directory moves their rows to that directory's own root, where they are rebuilt, so a restored daemon never adopts a running daemon's workspaces (S-186). A symlinked `~/.reef` is followed (S-187).
- A daemon given its own state directory is unchanged, so tests and second daemons never write to `~/.reef`.
