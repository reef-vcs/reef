# 0053. Workspace provider contract

- Status: accepted
- Date: 2026-11-01
- Ticket: R2.26

## Context

Coding-agent harnesses isolate parallel sessions in directories of their own, usually git worktrees. A harness that could ask Reef for those directories would give its sessions Reef's isolation, snapshots and reclaim without a change to its own workflow. At R2.26 one harness documents a command that replaces its worktree creation, with JSON in and the directory's path out. Another replaces it only through an in-process plugin, and the rest hard-code `git worktree add`. Reef needs one interface any of them can call, and a way to keep a workspace alive as long as the session that asked for it, which is neither `ws run`'s child process nor the MCP server's.

## Decision

1. **Three leaves under `ws`:** `reef ws provider create | attach | release`. Each takes its fields as flags, or as one JSON object on stdin keyed by the same names (`--stdin`), with `--at FIELD=POINTER` (RFC 6901) taking a field from anywhere in that object. A flag wins over a pointer, a pointer over a key, a key over the default. Each prints one object with `contract: 1`, or the path alone with `-o text`. Two CLI reasons: `PROVIDER_INPUT_INVALID`, with `metadata.field`, and `HOLDER_NOT_RUNNING`. Local only.
2. **The verbs map onto the existing RPCs.** No API change. `create` is `CreateWorkspace`. With a `name`, it returns the active workspace whose owner is `<user>@<host>:<name>` if there is one, so a retry or a repeated name gets the same directory. From a path inside an active workspace, it starts on that workspace's commit after a snapshot. `attach` is `ws run --workspace`'s rule: active, a live lease, a permitted owner, renewed once. `release` is `ReleaseWorkspace` with a final snapshot, and by id it is idempotent, a release or reclaim in flight included; with `hold_while` it releases for that holder only, keeping the workspace while another holder runs. A name is used as given when it is a safe owner suffix and made one otherwise; on a name miss, a name whose last workspace ended starts on its kept change. `name` and `owner` together are refused, since a name is held through its owner.
3. **A client-side follower holds the lease while the holder runs**, one per holder; a workspace is held while any of its holders runs, and no holder takes it from another. `hold_while` names a pid, or `parent`, the nearest non-shell ancestor. `create` and `attach` spawn a detached `reef ws renew --follow --while-pid <holder>`, which renews on the daemon's cadence with `ws run`'s `Heartbeater` and exits when the holder exits (pid and start time), when the lease ends, or on a signal. Nothing is released at the holder's death: the lease runs out and the daemon reclaims the workspace with a final snapshot, as for R2.13's MCP server.
4. **A holder record** per follower, at `<state>/holders/<workspace id>.<follower pid>.json` (mode `0700` directory), names the follower and the holder while the follower runs, so a follower only ever writes and removes its own. It is written after the first renewal, which is retried while the daemon is away, and removed when the follower exits or is signalled; a killed follower's record is removed by the next reader. A record counts only while its follower runs and its lease is current. `attach` reads it to replace a holder, `release` to stop the follower at once, and `reef here` to report `held: true` and stop telling an agent to release a workspace its harness holds.
5. **Workspaces live at the daemon's path**, never inside the checkout or at a path the harness suggests.
6. **Adapters are configuration.** The contract document, `docs/workspace-provider.md`, names no harness. A harness's adapter is facts in Reef's agent table, rendered into its package format, and opt-in.

## Alternatives considered

- **Flags on the agent-facing `ws create`, `ws run` and `ws release`.** Their help is what agents read, near its byte budget ([0051](0051-task-first-help.md)). The agent would pay for plumbing it never passes, and the agent contract would be tied to the harness contract.
- **A top-level noun.** The root help is 509 of its 512 bytes and must name every top-level command.
- **A documented protocol over the existing commands.** A hook gets JSON on stdin and cannot turn it into argv without `jq`. Nothing existing holds a lease for the life of another process.
- **The MCP server as the provider.** Its results are JSON, not a path, and a harness may start its sessions before its MCP servers are up.
- **The daemon watching a pid.** A daemon change for what a client can do, lost when the daemon restarts.
- **Releasing when the holder dies.** A restarted or resumed session would find its directory gone.
- **A long lease instead of a follower.** A crashed session's directory would hold disk for the whole lease, and a session longer than the lease would lose it.
- **Creating at the harness's suggested path.** Inside the checkout, a harness's own check refuses it, and the root's jj and git see it as untracked files.
- **One package for everything.** Replacing worktrees changes what a session's isolation is (no branch, work as a jj change), which installing Reef's skill and MCP server must not do as a side effect.

## Consequences

- Any harness that can run a command when it makes and removes a session's directory can use Reef's workspaces with configuration alone.
- A workspace of a dead session lives up to the lease plus its grace (35 minutes by default), then goes with a final snapshot.
- Each held workspace costs one idle follower process: one renewal per `heartbeat_every` and one process check every 2 s.
- `reef here` reads one more file.
- A harness's git-based checks see nothing in a Reef workspace; the contract document leads with it.
