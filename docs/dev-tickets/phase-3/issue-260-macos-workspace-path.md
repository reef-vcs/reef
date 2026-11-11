# Issue #260: a short, space-free default workspace root

Phase 3 · Size S · Fixes: issue #260 · Builds on: R1.04 (configuration and the state directory, ADR 0013), R1.02 and R1.17 (the base cache), R1.23 (the abandoned-create sweep), RX.01 (S-01, private directories) · Unblocks: nothing; removes the "known workspace artifact" that four ticket PRs (#184, #236, #241, #251) listed

## Context

On macOS a workspace lives at `~/Library/Application Support/reef/repos/<repo-id>/ws/<ws-id>`. For the user who filed the issue that is 89 bytes with a space in it:

    /Users/arsood/Library/Application Support/reef/repos/a3dda866c7e8f343/ws/b7eee08cd3560390

- **The space** breaks every unquoted shell use (`cd $(reef ws create)`, `$REEF_WORKSPACE_PATH` in a script, `docker -v`, `xargs`, URL-ising tools: `uv` saw `%20`; `reef-vcs/src/cli/symbols.rs:286` already works around it for LSP URIs). Agents write unquoted paths often, and so do repositories' own scripts (#184 found two such tests).
- **The length** leaves 14 bytes under macOS's 103-byte socket path limit (`reef_client::paths::SOCKET_PATH_MAX`). Any test or tool that makes a socket under the workspace fails: Reef's own `config::tests::empty_env_is_unset_and_relative_is_absolutized` is a "known workspace artifact" in several PRs, and the Python SDK tests moved to a short temp dir.

Linux's default has no space but the same length problem: `/home/<user>/.local/state/reef/repos/<16>/ws/<16>` is 67 bytes plus the user name, leaving 40 bytes minus the name under a 107-byte limit.

### What was verified while writing this ticket

At main `0d14bf1e` (API revision 30, schema 17, last ADR 0070 with 0071 taken by #259, last threat row S-176 with S-177–S-184 reserved).

- **The root is a template.** `[workspace] root` defaults to `"<state>/repos/<repo-id>/ws"` (`reef-daemon/src/config.rs:494`); `validate` requires `<repo-id>` and `<state>` only at the start (`:2286`); `Config::workspace_root(repo)` substitutes both, a relative result taken against the state directory (`:1567`).
- **Rows record each workspace's path.** `new_paths` (`workspace/create.rs:361`) computes `<root>/<ws-id>` once, refuses an existing path or a path a row holds, and the row keeps it; release, snapshot, inspect and reclaim use the row's path. So existing workspaces keep working wherever the root moves.
- **Three start sweeps read the root, not rows**: the trash sweep (`daemon.rs:595`, `*.trash-*` and probe clones), the abandoned-create sweep (`workspace/abandoned.rs:57`, directories named like an id with no row), and Linux's overlay reconcile (`daemon.rs:787`, its scan directories). After a root change they would miss the old root.
- **Forgetting a repository** removes `<state>/repos/<id>/` recursively (`workspace/manager.rs:469`), which today also takes its workspace trash. It is refused while a workspace is active.
- **Private directories.** `daemon::ensure_private_dir(dir, create)` (`daemon.rs:1014`) refuses a directory another uid owns (root included), tightens group/other bits to `0700` with a warning, and creates missing ones `0700`. S-01 applies it to the state and socket directories at every start; the workspaces were covered only by being inside the state directory.
- **Backends.** The APFS backend clones bases with `clonefile`, which needs the base and the workspace on one volume; `probe_apfs(Some(bases), target)` probes exactly that pair and the selection falls back to copy, and `reef daemon status` lists the outcome. `~/Library/Application Support` and any directory directly under `~` are on the same Data volume in a default install; Linux's reflink tier (same filesystem) behaves alike.
- **Repository ids are generated in one place**, `repo/registry.rs:287`; ids are 64 random bits as 16 hex (`model/ids.rs`), and the CLI accepts prefixes of four or more.
- **Daemon tests set `REEF_STATE_DIR`** (`tests/common/mod.rs:88`), so they never use the platform default state directory.
- **`reef config show`** prints `Config::to_effective_toml` (`offline.rs:194`), resolved values with a `# source:` comment, as `daemon.socket` already is.

## Decisions

### The default root (decisions 1 to 3)

When the state directory is the platform default (its source is `default`) and `config.toml` sets no `[workspace] root`, the root is

    ~/.reef/<repo-short>          workspaces at ~/.reef/<repo-short>/<ws-id>

on **both** macOS and Linux, where `<repo-short>` is a new template placeholder: the repository id's first 8 hex characters. For the reporter: `/Users/arsood/.reef/a3dda866/b7eee08cd3560390`, 45 bytes, 58 to spare. The rule: at least 50 bytes left under 103 for any home of `/Users/` or `/home/` plus a user name of up to 14 bytes.

- **Why not keep `ws/` or the full repository id.** `~/.reef/ws/<16>/<16>` leaves 47 bytes for this user; `~/.reef/<16>/<16>` leaves exactly 50 and less for any longer name.
- **The workspace directory keeps the full 16-hex id**: the name an agent sees is the id every command takes, and the abandoned-create sweep finds directories by parsing that name.
- **No readable prefix** (repository name): its length varies with the repository and breaks the budget; `reef here` and `REEF_WORKSPACE_PATH` already name the repository.
- **A state directory given explicitly** (`REEF_STATE_DIR`, `--state-dir`) keeps `<state>/repos/<repo-id>/ws`, the rule ADR 0013 already uses for the `$XDG_RUNTIME_DIR` socket: a second daemon, and every test, stays inside its own directory and never writes to `~/.reef`.
- **The base cache does not move.** It stays at `<state>/repos/<id>/bases`, on the same volume as `~` in a default install; the existing probe already falls back to copy when it is not (a symlinked `~/.reef` on another volume, say).

### Unique `<repo-short>` (decision 2)

Registration draws a new id until no registered repository shares its first 8 hex characters (bounded, say 16 draws). Repositories registered before this ticket could share a prefix (about 1e-8 for ten repositories); then they share one directory, which is harmless: workspace ids are unique across repositories, the sweeps look rows up by id across all repositories, and forgetting a repository (below) removes only trash and an empty directory. An abandoned create one repository's sweep finds in a shared directory may be removed by the other's sweep, which logs a failed jj forget; the owning repository's sweep still forgets it through its `wc/<id>` directory.

### Migration (decision 4)

No data moves. Existing workspaces keep their recorded paths and keep serving and releasing. New ones go to the new root. `Config::workspace_roots(repo)` returns the effective root and, when it differs, the old default `<state>/repos/<repo-id>/ws`; the three start sweeps use both, so trash and abandoned creates under the old root are still cleaned up. A configured `root` is unchanged and nothing about it changes. A user who changes a configured root keeps today's behavior (the old one is not swept).

### Ownership and permissions (decision 5)

The default root's base, `~/.reef`, gets S-01's treatment:

- at every start, `ensure_private_dir(~/.reef, create = false)`: another uid's directory refuses the start (exit 7, hint naming `[workspace] root` and the path), group/other bits are tightened to `0700` with the usual warning;
- at each create, `~/.reef` and `~/.reef/<repo-short>` are created `0700` when missing (`ensure_private_dir(.., true)`).

A configured root gets no new check: the user chose it, may share it on purpose, and chmod-ing it would surprise. The threat model documents that.

### Forgetting a repository

`forget_repository` also clears `~/.reef/<repo-short>` under the default root: it removes `*.trash-*` entries, then the directory with `remove_dir` (only if empty), never recursively, since the directory might be shared (above).

### Discovery (decision 6)

- `reef config show` prints `root = "/Users/you/.reef/<repo-short>"  # source: default` (resolved, as the socket is). The effective TOML still loads back to the same configuration.
- The quickstart replaces "The state directory holds the daemon's database, logs, and workspaces" with where workspaces are, on both platforms, and how to see it (`reef config show`).
- No API change: `reef daemon status` is unchanged.

### ADR and threat rows

ADR **0072** "Default workspace root outside the state directory", short, amending ADR 0013's layout paragraph (which gets a pointer). Threat rows:

- **S-185** Default workspace root, another local user, High: a `~/.reef` another uid owns, or with group/other bits. Fixed: checked at start and create as above. Tests below.
- **S-186** Workspaces outside the state directory, Low: they are no longer covered by the state directory's `0700`, by `restore --into`'s path relocation, or by forget's recursive removal. Bounded: `~/.reef` is `0700`; restore rematerializes at recorded paths; forget removes trash and an empty directory only.
- **S-187** A symlinked `~/.reef`, same uid, Low: followed; the owner and mode checked are the target's; a target on another volume makes the APFS/reflink probe fall back to copy. Documented.

The asset table's state-directory row loses `repos/*/ws` and gains a "Default workspace root" row.

## Deliverables

- `crates/reef-daemon/src/config.rs`: `REPO_SHORT_PLACEHOLDER`; the default root resolved at load from the state directory's source and home; `workspace_root` substitutes `<repo-short>`; `workspace_roots`; validation accepts `<repo-id>` or `<repo-short>`; module docs' layout. `crates/reef-client/src/paths.rs`: the home directory alongside `PlatformDefaults` if needed, and the table.
- `daemon.rs`: the start check, sweeps and reconcile over `workspace_roots`. `workspace/abandoned.rs`: both roots. `workspace/create.rs`: private parents. `workspace/manager.rs`: forget. `repo/registry.rs`: the unique-prefix draw.
- Docs: ADR 0072, ADR 0013 pointer, `docs/threat-model.md`, `docs/quickstart.md`, `crates/reef-daemon/README.md` if it shows the layout, `CHANGELOG.md` Upgrading entry.

## Tests

1. `config::tests::default_workspace_root_fits_a_socket`: for homes `/Users/abcdefghijklmn` and `/home/abcdefghijklmn` and a random repository and workspace id, the default workspace path has no whitespace and leaves at least 50 bytes under `SOCKET_PATH_MAX` (the acceptance test).
2. `config::tests`: the default applies only to a default state directory; an explicit state directory keeps `<state>/repos/<repo-id>/ws`; a configured root wins either way; `<repo-short>` substitutes; validation; `config show` round trip.
3. Registration: the draw skips a taken prefix (a scripted id generator).
4. **Upgrade** (`tests/workspace_root.rs`, daemon binary): start with the old layout, create workspace A; stop; set `root` to a short template with `<repo-short>`; plant `x.trash-1` and an id directory with no row under the old root; start: A still snapshots, inspects and releases from its old path; a new workspace B is under the new root; the planted entries are gone.
5. S-185: `~/.reef` owned by another uid refuses start (privileged test as S-01's), group bits tightened, created `0700`. The home is injected through a test hook, never the real `~`.
6. Forget removes trash and an empty `<repo-short>` directory and leaves a non-empty one.

## Acceptance criteria

- A default workspace path has no space and leaves at least 50 bytes for a socket name under the limit; test 1 asserts both.
- RX.01's ownership checks apply to the new root (test 5, S-185).
- A daemon with workspaces under the old root keeps serving and releasing them (test 4).
- `reef config show` and the quickstart say where workspaces are.

## Out of scope

Moving existing workspaces; shortening workspace ids; Spotlight/Time Machine exclusion of `~/.reef` (a #264 laptop-budget question); the root daemon's `REEF_STATE_DIR=/var/lib/reef` layout (explicit state directory, unchanged).

## Risks

- `~/.reef` may already exist for an unrelated tool; Reef only adds `<8-hex>` directories in it, and the ownership check refuses only another uid's directory.
- A wrongly computed home (`HOME` unset) is today's `NoHome` error for the default state directory; the root uses the same home.

## Decisions for the user

Settled 2026-11-11: the user accepted all ten recommendations as written. (1) `~/.reef/<repo-short>/<ws-id>`; (2) registration redraws until the 8-hex prefix is unique, earlier collisions share a directory; (3) the full 16-hex directory name, no readable prefix; (4) only for the platform-default state directory, on macOS and Linux; (5) nothing moves, the start sweeps cover both roots, a configured root is untouched; (6) the base cache stays; (7) `~/.reef` is `0700` and checked at every start, no new check for a configured root; (8) forget removes only trash and an empty directory; (9) `reef config show` and the quickstart, no API change; (10) ADR 0072 with a pointer in ADR 0013.
