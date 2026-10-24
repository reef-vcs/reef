# Threat model

What Reef protects, from whom, and where it does not. RX.01 wrote this for the 0.1.0 release; [ADR 0027](adr/0027-trust-boundaries-in-phase-1.md) is the decision it rests on. It is a living document: a ticket that changes a trust boundary (a new RPC, a new file the daemon writes or runs, a new place it reads repository content) adds its rows to the [findings table](#findings), and [`SECURITY.md`](../SECURITY.md) points reporters here.

## The boundary in one paragraph

In Phase 1, one user id is one trust domain. The daemon runs as the user, and anything running as that user can reach its socket. An agent in a workspace can therefore do everything the user can: read and write every workspace, the user's checkout, the shared stores, and the state directory, and call every RPC. Workspaces isolate *accidents* (a separate directory, a separate working-copy commit, build outputs out of the tree, a lease that cleans up) and *repository content* (a tree, a `reef.toml`, a symlink), not a hostile agent. Containing a hostile agent needs a sandbox outside Reef: the harness's own, a user namespace, or a microVM. The boundaries Reef does enforce are the kernel's, against other local users, and the ones around repository content, which Reef acts on before the user runs anything from it.

## Assets

| Asset | Where | Why it matters |
| --- | --- | --- |
| The agents' work | jj changes and operations in `.jj/repo`, workspace directories | Losing it is what Reef exists to prevent |
| The user's checkout | The registered root, its `.git` and `.jj` | Reef writes `.jj` and one `info/exclude` line there (ADR 0024) |
| The state directory | `<state>`: `reef.sqlite3`, `logs/`, `repos/*/bases`, `repos/*/ws`, `stores/` | Holds every workspace's lease id, paths, and the base cache; the stores are mounted into every workspace |
| The socket | `<state>/daemon.sock`, or `$XDG_RUNTIME_DIR/reef/daemon.sock` | Every RPC is authorized by reaching it |
| Backups | Any directory `reef backup` names | A full copy of the database and the jj repositories |
| The binaries | Release tarballs, the Homebrew formula, `install.sh` | 0.1.0 is the first thing anyone installs |

## Actors

| Actor | In scope for 0.1.0 | Why |
| --- | --- | --- |
| A same-uid agent in a workspace, running arbitrary code | Only as the subject of claims: no document promises protection against it that the code does not enforce | Not a boundary (ADR 0027) |
| Another local user on the same host | Yes | The one boundary the kernel gives us: modes and ownership |
| Repository content: a repository the user cloned, or a commit an agent made (`reef.toml`, tree entries, symlinks, `.git/config`) | Yes | Reef acts on it with the user's privileges before the user runs anything from it |
| A compromised package fetched by a store build | Documented only | The same as running the install yourself; Reef cannot vet it |
| A network attacker against the installer, and the release pipeline | Yes | 0.1.0 is the first thing anyone installs |
| A remote client (R2.08) | Requirements only | No listener exists; [the requirements](#requirements-for-a-remote-listener) say what one must satisfy |
| Forged evidence, keys, executors | No | RX.02, Phase 3 |

## Boundaries and what each component trusts

- **The socket** is `0600` in a `0700` directory the daemon's user owns, so only that uid connects. The daemon reads no peer credentials: every client is the user, and every RPC is open to every client. A root daemon (the kernel overlay tier) serves only root clients, so its agents are root too.
- **The state directory** is `0700` and owned by the daemon's euid, checked at every start; the database is `0600`. A state directory another uid owns is refused, root included (S-01).
- **Repository registration** walks up from the given path to the nearest `.jj` or `.git` and accepts it only if that directory is owned by the daemon's euid, or under `sudo` by `SUDO_UID`, git's `safe.directory` rule (S-02).
- **Repository content** is trusted to be data. Materialization goes through jj-lib's checkout, which refuses `.`, `..`, `.git`, and `.jj`; Reef refuses a commit with a tree entry jj-lib cannot read before jj reads it (S-18); every walk Reef does itself refuses to follow symlinks; store mounts check every ancestor of the mount path; ignore files are patterns, never paths the daemon opens. The tests are `crates/reef-daemon/tests/hostile_repo.rs`.
- **`reef.toml`'s `build`** is the one piece of repository content the daemon executes, as the repository's declared build command, and only when `Stores.Build` asks for it. The CLI shows the argv and `[store.env]` before the output; `[store] allow_builds = false` refuses every build (S-03).
- **The lease id** authorizes heartbeats, and the owner string is informational. Any client can list every lease id, and `Release` needs none: the socket is the authorization (S-08).
- **`git gc`** runs `[gc] git_executable` in a registered repository, which reads that repository's `.git/config`. For a repository the user owns, that is git's behavior, outside this model (S-13).
- **The installer** fetches over HTTPS only, checks each tarball against the release's `SHA256SUMS`, and installs without `sudo`. The checksums come from the same release, so they prove the download is what GitHub serves, not who built it; the provenance attestation does that (S-06).

## Harness sandboxes

Harness sandboxes (Claude Code's, Codex's) confine an agent's writes and network access. None of them knows about Reef's socket. If the sandbox lets the agent connect to it, `Create.path`, `Backup.dest`, and `Stores.Build` act outside the sandbox with the user's full privileges: a store build runs the argv of the agent's choosing (it can commit its own `reef.toml`), and a workspace created at a path of the agent's choosing writes a tree of the agent's choosing there. This is the one-uid decision seen from inside a sandbox: Reef is not the sandbox's enforcement point and cannot tell the sandboxed agent from the user (S-07).

What an operator can do:

- Deny the sandboxed agent the socket: `<state>/daemon.sock` (`~/Library/Application Support/reef/daemon.sock` on macOS, `~/.local/state/reef/daemon.sock` on Linux), `$XDG_RUNTIME_DIR/reef/daemon.sock` on Linux, and whatever `REEF_SOCKET` names. The agent then cannot use Reef at all.
- Or let it use workspaces but never trigger a build: `[store] allow_builds = false` in the daemon's `config.toml`, and build stores outside the sandbox. `Create.path` and `Backup.dest` stay open; give the sandbox no daemon whose user can write where it must not.

The follow-up that would remove daemon-side execution altogether ([#61](https://github.com/reef-vcs/reef/issues/61)) is to run the build in the client's process, inside its sandbox, with the daemon only sealing the result. It changes ADR 0022's second decision and is a Phase 2 question, with R2.13's MCP server.

## Findings

Severity is the rubric's:

| Severity | Meaning |
| --- | --- |
| Critical | Repository content gets code execution, or a write or delete outside the workspace and state directory, from an operation that does not ask for that repository's code to run (create, snapshot, release, reclaim, gc, backup, registration); or another local user reaches the socket, state, or workspaces |
| High | Another local user reads Reef state or a workspace, or plants something Reef then acts on; repository content makes one of those same operations act outside the workspace and state directory; a tampered binary installs over HTTPS without a checksum failure |
| Medium | A documented promise the code does not keep; repository content crashes or wedges the daemon; an operation that does ask for the repository's code to run (`reef store build`) does so without showing the command or without an operator's switch; a same-uid process doing something the documents imply it cannot |
| Low | Hardening with no concrete actor behind it |

What blocks a release: every Critical and High is fixed or unreachable by default; every Medium is fixed, or listed in [the quickstart's limitations](quickstart.md#known-limitations-in-phase-1) and here; Low is filed.

Status is *fixed in RX.01* with the tests, *verified* (the test shows Reef already does the safe thing), *documented* with where, or *follow-up*.

| Id | Area | Actor | Severity | Finding | Status |
| --- | --- | --- | --- | --- | --- |
| S-01 | State directory | Another local user | High | The daemon left an existing state directory's mode and owner as it found them, and created the database with the umask's mode (`0644`), private only because its directory was. A directory another user created (a shared `REEF_STATE_DIR`, a reused scratch volume) would have been used as it was. | Fixed in RX.01: at every start, before the lock is taken, a state directory or socket directory another uid owns is refused (exit 7, the hint names `REEF_STATE_DIR` or `REEF_SOCKET`), with no exception for root, so a `REEF_SOCKET` directly in `/tmp` is refused too; one with group or other bits is `chmod`ed `0700` with a `warn` line; the database and its journal files are `0600`; the log directory, a backup's existing empty destination (its database `0600` too), and a restore target get the same checks. Tests: `daemon::state_dir_with_group_or_other_bits_is_tightened`, `daemon::state_dir_owned_by_another_user_refuses_start`, `daemon::root_state_dir_owned_by_the_sudo_user_is_refused`, `daemon::database_file_mode_is_0600`, `daemon::existing_database_is_restricted_at_start`, `backup::backup_dir_and_database_are_private` |
| S-02 | Registration | Another local user | High | Implicit registration walks up to `/`, so a `.git` another user planted in a shared directory (`/tmp`, a CI scratch volume) would be registered, written to (`.jj`, `info/exclude`), have its trees materialized, and have `git gc` run with its `.git/config`. | Fixed in RX.01: the `.jj`, or the `.git` jj would be initialized beside, must be owned by the daemon's euid or, for euid 0, the decimal `SUDO_UID`; otherwise `PermissionDenied` with a hint naming `[repo] allow_foreign_owner = true`. A `.jj` or `.git` that is a symlink must have an accepted owner too, as must its target, so another user's `/tmp/.jj` pointing at the user's repository does not make `/tmp` its root. Tests: `integration::register_refuses_a_root_owned_by_another_user`, `integration::register_walks_up_only_to_an_owned_root`, `integration::register_accepts_the_sudo_users_repository`, `integration::register_accepts_the_sudo_users_repository_through_the_daemon`, `repo::registry::tests::*` |
| S-03 | Store builds | Repository content | Medium | `reef store build` ran the repository's declared argv without showing it, and nothing could turn builds off. | Fixed in RX.01: `BuildEvent.started` carries the argv, `[store.env]`, and the commit before any output; the CLI prints `reef: building <name> at <rev>: <argv>` and `reef: env: ...` on stderr in both modes, control characters escaped; the daemon logs it at `info`; `[store] allow_builds = false` makes `Stores.Build` `PermissionDenied`. Tests: `stores::build_is_refused_when_builds_are_off`, `stores::build_announces_its_command`, `cli::store_build_prints_the_command_first`, `cli::store::tests::started_line_quotes_the_argv_and_lists_the_env` |
| S-04 | Privilege | Same-uid agent | Medium | Nothing said that a root daemon makes every client root and runs store builds as root. | Fixed in RX.01: a root daemon logs a `warn` line at start saying so; the quickstart says it, and to set `REEF_STATE_DIR` for a root daemon. `workspace.overlay_nosuid` keeps its default: in the kernel tier the agent is root already, so `nosuid` buys nothing until daemon-user separation exists. Test: `daemon::root_daemon_warns_at_start` |
| S-05 | Documents | Same-uid agent | Medium | `docs/design.md` said an agent "cannot poison another agent's workspace", which the symlink tier does not enforce. | Fixed in RX.01: the design, the quickstart's limitations, and `SECURITY.md` state the boundary as this document does. |
| S-06 | Release | Network attacker, release pipeline | Low | The checksums prove only that a download is what GitHub serves; nothing attests who built it. | Follow-up (R4.17): the publish job attests the tarballs and `SHA256SUMS` (`actions/attest-build-provenance@v4`), and the quickstart shows `gh attestation verify`, but dry runs skip the publish job, so no run has exercised the step yet. 0.1.0's release run is the first; R4.17 owns it from there. |
| S-07 | Socket | Sandboxed same-uid agent | Medium | A harness sandbox that lets its agent reach the socket lets it act outside the sandbox through `Create.path`, `Backup.dest`, and `Stores.Build`. | Documented: [Harness sandboxes](#harness-sandboxes) and the quickstart's limitations name the socket paths to deny and `[store] allow_builds = false`. Follow-up: client-side store builds, [#61](https://github.com/reef-vcs/reef/issues/61) (Phase 2, with R2.13); a sandbox for builds, [#62](https://github.com/reef-vcs/reef/issues/62). |
| S-08 | Leases | Same-uid agent | Medium | The lease id is the capability and the owner string is informational: any client lists every lease id (`List`, `Inspect`), `Release` needs none, and `ws run --workspace` attaches by owner string. | Documented: here and ADR 0027. Authorization is the socket. |
| S-09 | API | Same-uid agent | Medium | `Create.path` places a workspace root, and `Backup.dest` a backup, at any absolute path the user can write. | Documented: here and S-07. |
| S-10 | API | Same-uid agent | Low | Any client can `Quiesce` the daemon, pausing every repository's writes up to `backup.quiesce_max`. | Documented: here. Filed: [#64](https://github.com/reef-vcs/reef/issues/64). |
| S-11 | Store builds | Repository content | Medium | `[store.env]` sets any variable, `PATH` and `HOME` included, and the build has the network. | Documented: here and the quickstart's limitations. `[store.env]` is weaker than `build` itself; the network is how a build fetches. |
| S-12 | Stores | Same-uid agent | Medium | Without `CAP_SYS_ADMIN` (macOS, unprivileged Linux) a store is sealed by `chmod` only, which the same user can undo, and a store is shared by every workspace mounting it. | Documented: the quickstart's limitations, the design's store paragraph, ADR 0022. |
| S-13 | GC | Repository content | Low | `git gc` in a registered repository reads its `.git/config` (`core.fsmonitor`, hooks, aliases), which can run commands. | Documented: here. For a repository the user owns this is git's behavior; S-02 keeps other users' repositories out. Filed (hardening): [#64](https://github.com/reef-vcs/reef/issues/64). |
| S-14 | Release | Network attacker | Low | The binaries are not signed or notarized. | Documented: the quickstart. R4.17. |
| S-15 | Snapshots | Same-uid agent | Low | Watcher snapshots keep every edit, secrets an agent wrote included, in the local operation log until GC. | Documented: the design's durability section and the quickstart's limitations (snapshots are local). |
| S-16 | Registration | Root daemon | Low | Under `sudo -E`, S-02 lets a root daemon initialize jj beside the user's `.git`; the `.jj` and the `info/exclude` line are root's, so the user's own `jj` then fails there with `EACCES`. | Documented: here; the quickstart advises a root daemon only in a VM or container dedicated to the agents. Filed: [#64](https://github.com/reef-vcs/reef/issues/64). Test: `integration::register_accepts_the_sudo_users_repository` asserts the owner. |
| S-17 | Materialization | Repository content | — | A tree entry named `..` holding a file. | Verified: the checkout fails (`Invalid path component ".."`), no workspace directory is left, nothing is written outside, and the daemon answers. Test: `hostile_repo::tree_entry_named_dotdot_fails_the_create_cleanly` |
| S-18 | Materialization | Repository content | Medium | A tree entry whose name contains `/` (or is empty) panics jj-lib 0.46 in `GitBackend::read_tree`, which unwraps `RepoPathComponentBuf::new` while holding the git backend's mutex. The poisoned mutex made every later read panic, for every workspace of the repository, until the daemon restarted. | Fixed in RX.01 on Reef's side: the daemon walks a named commit's trees with gix, outside jj's lock, and refuses one with such an entry (`InvalidArgument`) before anything reads it: a create's or move's start, a store build's revision, GC's default start, and a plain git checkout's `HEAD` before registration initializes jj beside it (which would otherwise panic mid-checkout and leave a half-made `.jj`). Commits read later without the check are a workspace's own, which were checked when it was created or moved, or written by jj from files on disk, which cannot hold such a name. Known-good trees are remembered, so a commit costs only its changed trees. As a backstop, a repository thread whose job panicked reopens the repository instead of reloading it. A workspace whose jj handle was opened before such a panic keeps the old store until the daemon restarts: [#63](https://github.com/reef-vcs/reef/issues/63). Upstream: [jj-vcs/jj#10466](https://github.com/jj-vcs/jj/issues/10466) (still on jj's `main` at 0306cff; plain `jj` 0.46 panics the same way after `jj new` on such a commit). Tests: `hostile_repo::tree_entry_with_a_slash_does_not_take_the_repository_down`, `hostile_repo::registering_a_checkout_whose_head_jj_cannot_read_fails_cleanly`, `hostile_repo::a_panicked_job_reopens_the_repository`, `reef_lib::jj::tree_names::tests::names_jj_cannot_read` |
| S-19 | Materialization | Repository content | — | Tree entries `.git` and `.jj`, as a file and as a directory. | Verified: jj-lib refuses each (`Reserved path component`), and nothing under the user's `.git` changed but the refs the test wrote and their logs. Test: `hostile_repo::reserved_names_in_a_tree_are_refused` |
| S-20 | Materialization, snapshots, release | Repository content | — | Tracked symlinks to `/`, to the directory above the state directory, and to an absolute canary directory. | Verified on every backend the probe offers: the links are recreated as links; disk usage, a snapshot after a write, the release, and the trash sweep after a restart leave the canary directory and the user's checkout untouched. Test: `hostile_repo::symlink_to_outside_is_recreated_and_never_followed` |
| S-21 | Stores | Repository content | — | A store mounted at `vendor/deps` in a tree where `vendor` is a symlink to a canary directory. | Verified: the create fails `Conflict`, nothing is written in the canary directory, and no workspace directory is left. Test: `hostile_repo::store_mount_under_a_symlinked_ancestor_is_refused_end_to_end` |
| S-22 | Snapshots, reclaim | Same-uid agent | Low | A workspace root replaced by a symlink was snapshotted through it: the final snapshot of its reclaim recorded the directory it pointed at as the workspace's files. The tear-down already unlinked the link without following it. | Fixed in RX.01: a snapshot of such a root is refused (`Conflict`), and its reclaim or release skips the final snapshot (`reclaims.detail`: "workspace root replaced by a symlink") and unlinks it. Tests: `hostile_repo::reclaim_of_a_root_replaced_by_a_symlink_removes_only_the_link`, `hostile_repo::snapshot_of_a_root_replaced_by_a_symlink_is_refused` |
| S-23 | Snapshots | Repository content | — | `[snapshot] exclude` and `.reefignore` lines that look like paths outside the workspace (`/etc/passwd`, `../x`, an absolute canary path). | Verified: they are patterns; creation, a snapshot, and the release work, and nothing outside the workspace is written. Test: `hostile_repo::reefignore_and_excludes_are_patterns_only` |

## Not boundaries in Phase 1

- **Sandboxing `reef ws run` or a store build** (user namespaces, seccomp, a build container): ADR 0016's alternative and ADR 0022's consequence. Scheduled when a harness asks for it, or when R3.06's executor sandbox exists to share: [#62](https://github.com/reef-vcs/reef/issues/62).
- **Daemon-user separation**: a `reef` system user owning the daemon and the stores, clients told apart by peer credentials. This is the step that would make the daemon a boundary against its own clients and make `workspace.overlay_nosuid` mean something; a Phase 2 or later decision, with R2.08.
- **Peer-credential checks on the socket**: the mode already limits clients to one uid, and the daemon has no second uid to tell apart.
- **Denial of service on the local socket**: rate limits and request size limits.
- **A shared state directory**: two uids sharing one never works (a root-owned database, socket, and lock refuse the other's daemon), and S-01 refuses it outright.

## Requirements for a remote listener

R2.08 adds a listener that is not the socket. Every RPC today is authorized by the socket's mode, so before it serves any of them, it must satisfy all of these, and its brief reviews its implementation against this list and adds its rows to the findings table:

1. **TLS**, with a pinned or CA-issued certificate. No plaintext mode.
2. **A per-client token with scopes.** A phone that approves proposals is not a client that can `Create` a workspace at an arbitrary path, run a store build, back up to a path, quiesce the daemon, or release another client's workspace. Each scope names the RPCs it allows; the default scope allows none of those.
3. **The local socket's full API is never exposed remotely by default.** What a remote client can call is an explicit list.
4. **Rate limits on the unauthenticated surface**: the handshake and token checks.
5. **Audit**: every remote mutation is recorded with the client's identity, in the operation log's description.

## Reviews

RX.01 had no external review. The pre-release has no users to protect yet, the one-uid model leaves a reviewer little that the tests here do not assert, and the budget is better spent on RX.02, where identity and signatures appear. RX.02 is the first external review.
