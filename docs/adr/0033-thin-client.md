# 0033. Thin client

- Status: accepted
- Date: 2026-10-26
- Ticket: R2.05

## Context

Design principle 8 is "API first, CLI second": the daemon owns snapshots, workspaces and queries, and the CLI is a thin client of its API. [0007](0007-crate-layout.md) gave the CLI its own crate so the dependency graph could show that, but left `reef-vcs` → `reef-daemon` → `reef-lib`, so `reef` linked jj-lib and the whole daemon. R2.01 ([0029](0029-daemon-api-v1.md)) moved every command onto `reef.api.v1`, yet the client stayed in the daemon crate, and three commands still ran daemon code in the CLI's process: `reef daemon run` (the daemon itself), `reef restore` (offline by [0020](0020-local-backup-format.md)), and `reef config show`, whose `Config` imports jj-lib's settings. Every other command loaded the same `Config` to find the socket. `cargo tree -p reef-vcs -e normal` listed 398 crates.

The MCP server (R2.13) ships in `reef`, and the SDKs (R2.06, R2.07) mirror the client, so where the client lives, and what it may depend on, is settled before they start.

## Decision

1. **Seven crates.** Two are new:
   - `reef-error` holds the error conventions ([0009](0009-error-and-logging-conventions.md)): `ErrorKind`, `Classify`, `classify_by_source`, `ErrorChain`, and `Advice` ([0025](0025-agent-facing-cli-contract.md)). It depends on nothing, and `reef-lib` re-exports it as `reef_lib::error`.
   - `reef-client` holds everything a process needs to talk to a daemon without being one: `Client`, the CLI's stable JSON, the client half of the error model, the path layering, stderr logging, and the backup manifest. It depends on `reef-api` and `reef-error` and no other Reef crate. The daemon re-exports it as `reef_daemon::client`.

   The direction is `reef-vcs` → `reef-client` → `reef-api`, `reef-error`; `reef-daemon` → `reef-client`, `reef-lib`, `reef-api`; `reef-lib` → `reef-error`. `reef-vcs` re-exports `reef_api` and `reef_client`, not the daemon or the library.

   R2.22 adds an eighth crate, `reef-broker` ([0038](0038-subscription-broker.md)), which depends on no Reef crate; `reef-daemon` depends on it.
2. **No jj-lib in `reef`.** Nothing that links jj-lib, nor `reef-lib`, `reef-daemon`, gix, rusqlite, notify or the Watchman client, is reachable from `reef-vcs` or `reef-client` over normal and build edges on any target. `reef-testing`'s `crate_graph` test walks `cargo metadata`'s resolve graph and names the path to the first one it finds. Dev-dependencies are free to use them.
3. **The daemon binary does what needs the daemon library.** `reef daemon run` `exec`s `reef-daemon --foreground` with the path flags, before logging or a runtime start, so the process keeps its pid, stdio and exit code. `reef config show` and `reef restore` run `reef-daemon config show [--json]` and `reef-daemon restore …`, which write exactly one document on stdout: the result, or on failure the `{"error": …}` object `reef` prints for any error, with the kind's exit code. `reef` prints the result as it did, or reports the error as its own. `reef-daemon` is found beside `reef`, else on `PATH`; there is no in-process fallback, and a missing one is exit 3 naming the fix. [0012](0012-daemon-process-model-and-threading.md)'s in-process start remains for tests.
4. **One answer for the socket.** The daemon's `Config::load` settles its paths through `reef_client::paths::Layered`, and the client through `ClientConfig::load`, which calls the same functions. The client reads three keys of `config.toml`, `daemon.socket`, `daemon.pid_file` and `repo.auto_register`, and ignores the rest: the daemon owns, and validates, its configuration ([0013](0013-daemon-state-directory-and-configuration.md)).
5. **The agent name.** `reef --agent <NAME>`, else `REEF_AGENT`, is sent as `reef-agent` on every call, so every operation the CLI causes names it (R2.03); with neither, the daemon records the workspace's owner. `ws run` passes it on to its command as `REEF_AGENT`. It is checked before any call. The owner, who holds a lease, stays separate from the agent, who caused an operation.
6. **Three reasons** make not-found errors machine-readable: `REPOSITORY_NOT_REGISTERED` (metadata `path`), `REPOSITORY_NOT_FOUND` (`repo`), `WORKSPACE_NOT_FOUND` (`workspace`). Implicit registration (R1.25) reads the first instead of the message. Reasons are constants, not schema: the API revision stays 3.

## Consequences

- `reef` depends on 120 crates instead of 398. A build of the CLI compiles no jj-lib, and the MCP server (R2.13) is added to a binary that links none.
- Two binaries must be installed together. The release tarball, `install.sh` and the Homebrew formula already ship both; installing from source names both crates (`cargo install … reef-vcs reef-daemon`), and `reef-daemon` has its own binstall metadata. A `reef` installed alone fails its first command that needs a daemon with exit 3 and the install line as its hint.
- A `reef-daemon` of another version on `PATH` can be the one `reef` runs. The add-only API and R2.01's restart rule cover a v1 daemon of another version; `reef daemon status` shows its version.
- A command other than `config show` no longer fails because of an unrelated bad key in `config.toml`; the daemon refuses to start on it, and `config show` names it.
- `reef config show` and `reef restore` spawn a process. Both are rare and interactive.
- 0007 is superseded. Its rule that the crate boundaries follow the process boundaries stands; the graph now shows it.

## Alternatives considered

- **A cargo feature that leaves the daemon out of `reef`.** jj-lib stays in the crate's graph and in default builds, and the enforcement becomes a feature matrix.
- **Keep the daemon in `reef` and ship a separate thin binary for agents.** Two commands to discover, rejected for the same reason [0025](0025-agent-facing-cli-contract.md) rejected a `reef-agent` binary.
- **`ErrorKind` in `reef-api`.** R2.01 forbids `reef-lib` → `reef-api`: the jj layer must not depend on tonic.
- **One `reef-core` crate holding both new crates' contents.** `reef-lib` would then depend on tonic.
- **`config show` as an RPC.** It must work with no daemon running, which is when it is most needed.
- **A second, client-only config loader.** Two copies of the path rules drift; one function means one answer.
- **Restore over the API.** Restore runs with the daemon stopped ([0020](0020-local-backup-format.md)).
