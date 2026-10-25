# reef-daemon

The local service of **Reef**, version control built for a world where AI agents write most of the code. It owns snapshots, workspaces, merge, and queries on behalf of agents, and hosts the typed API, workspace materialization backends, the lease manager, and filesystem watchers. It builds the `reef-daemon` binary.

The version-control data model and merge logic live in `reef-lib`, which this crate depends on. Its own model is the inventory it keeps in SQLite (repositories, workspaces, leases, reclaims), in `reef_daemon::model` and `reef_daemon::db`. It contains no CLI code.

`reef-vcs` depends on this crate and re-exports it. This crate never depends on `reef-vcs`. The design document and roadmap are at <https://github.com/reef-vcs/reef>.

Run it in the foreground with `cargo run -p reef-daemon` (state in the platform state directory, or `REEF_STATE_DIR`); it stops cleanly on Ctrl-C or SIGTERM. The process and threading model is [ADR 0012](../../docs/adr/0012-daemon-process-model-and-threading.md); the state directory and `config.toml` are [ADR 0013](../../docs/adr/0013-daemon-state-directory-and-configuration.md), and `reef config show` prints the effective configuration. The API it serves is `reef-api`'s (`crates/reef-api/proto/`, compiled at build time without `protoc`); it is described in [`docs/api.md`](../../docs/api.md) and [ADR 0029](../../docs/adr/0029-daemon-api-v1-and-the-reef-api-crate.md). Clients use `reef_api::client::Client`.
