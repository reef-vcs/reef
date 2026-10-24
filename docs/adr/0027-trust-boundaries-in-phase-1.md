# 0027. Trust boundaries in Phase 1

- Status: accepted
- Date: 2026-10-24
- Ticket: RX.01

## Context

Phase 1 ships a daemon that materializes repository trees, runs commands a repository declares, deletes directories, binds a socket, registers any git checkout a command runs in, and installs itself with `curl | sh`. Eight tickets left a security question for RX.01: store builds running arbitrary argv as the daemon's user, advisory sealing in the symlink tier, `[store.env]`, build network access, and no daemon-user separation ([0022](0022-shared-dependency-stores.md)); the lease id as the capability with an informational owner ([0019](0019-leases-and-reclaim.md)); no sandbox for `ws run` ([0016](0016-overlayfs-privilege-tiers.md)); `workspace.overlay_nosuid`; the socket's file mode as the first access control ([0004](0004-grpc-over-unix-socket.md)); and the threat model R2.08's remote listener is reviewed against. No threat model existed to answer them against. `docs/threat-model.md` is that model; this ADR is the decision it rests on.

## Decision

1. **One uid is one trust domain.** The daemon runs as the user, and its socket is `0600` in a directory only that user can enter, so every client is a process of that user. An agent in a workspace can therefore do everything the user can: read and write every workspace, the user's checkout, the stores (in the symlink tier), and the state directory, and call every RPC. The daemon is not a boundary against its own clients.
2. **What Phase 1 isolates.** Workspaces isolate accidents (a separate directory, a separate working-copy commit, build outputs out of the tree, a lease that cleans up) and repository content (a tree, a `reef.toml`, a symlink), which Reef acts on before the user runs anything from it. They do not contain a hostile agent; that needs a sandbox outside Reef, the design's "Sandboxed microVM" row or [0016](0016-overlayfs-privilege-tiers.md)'s user-namespace alternative.
3. **What Reef enforces.** Against other local users: the state directory, the socket's directory, and the log directory are the daemon's user's and `0700`, the database `0600`, checked at every start, root included; registration accepts only repositories the daemon's user (or the `sudo` user) owns. Against repository content: nothing it contains makes create, snapshot, release, reclaim, GC, backup, or registration write, delete, or mount outside the workspace, the base cache, or the state directory, or crash or wedge the daemon, as far as the hostile-content tests reach. `docs/threat-model.md`'s findings table records each check and its test, and a new way through is a finding.
4. **The API is authorized by the socket.** Every RPC is open to every client that can connect. A listener that is not the socket (R2.08) needs its own client identity and scopes before it serves any RPC; the threat model's "Requirements for a remote listener" lists what it must satisfy.
5. **The kernel overlay tier means everyone is root.** A root daemon serves only root clients, so its agents are root, and its store builds run as root. It logs that at start, and it is meant for a VM or container dedicated to the agents. `workspace.overlay_nosuid` keeps its default (false): with the agent already root, `nosuid` buys nothing. The flag stays for daemon-user separation, if that comes.
6. **Store builds.** `reef store build` runs the repository's declared build command, as `npm ci` runs its `package.json` scripts. A user or agent that runs it has asked for the repository's code, and under decision 1 the daemon running it is no escalation over the agent running it. The command is shown before its output, and `[store] allow_builds = false` refuses every build, for an operator whose agents run in a sandbox that can reach the socket: there the daemon running the build is worse than the agent running it. `[store.env]` and the build's network access are not restricted. Running the build in the client's own process, so it stays inside the client's sandbox, would change [0022](0022-shared-dependency-stores.md)'s second decision and is left to Phase 2.
7. **Leases.** The lease id stays the capability, and the owner informational. Any client can list lease ids and release any workspace. That is decision 1, not a gap to close in Phase 1.

## Consequences

- Answers [0019](0019-leases-and-reclaim.md)'s and [0022](0022-shared-dependency-stores.md)'s questions for RX.01 without editing them.
- The documents promise only what the code enforces: the design's store paragraph and the quickstart's limitations say workspaces are not a sandbox, and `SECURITY.md` points at the threat model.
- Two config keys, `[repo] allow_foreign_owner` and `[store] allow_builds`, let an operator relax registration or turn builds off.
- A root daemon started with `sudo -E` on the user's default state directory refuses to start; it needs a `REEF_STATE_DIR` of its own.
- The step that would make the daemon a boundary is daemon-user separation: a `reef` system user owning the daemon and the stores, clients told apart by peer credentials. That is a Phase 2 or later decision, with R2.08.
- Every later ticket that changes a trust boundary (a new RPC, a new file the daemon writes or runs, a new place it reads repository content) adds its rows to the threat model's findings table.

## Alternatives considered

- **Daemon-user separation now.** A system user, a setuid-free socket handshake, and peer credentials would make the stores truly read-only and the API authorizable per client. It needs an installer that creates users, which `curl | sh` without `sudo` cannot, and it changes every path. Not before there is a client that is not the user.
- **Refuse store builds by default.** No consent step that the socket could carry tells the user from the agent, so a default of off only moves the agent to running `npm ci` itself, which is the same command with the same privileges.
- **Peer-credential checks on the socket.** The socket's mode already admits one uid, and with one uid there is nothing for the credentials to distinguish.
