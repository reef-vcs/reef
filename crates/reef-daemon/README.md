# reef-daemon

The local service of **Reef**, version control built for a world where AI agents write most of the code. It owns snapshots, workspaces, merge, and queries on behalf of agents, and hosts the typed API, workspace materialization backends, the lease manager, and filesystem watchers. It builds the `reef-daemon` binary.

It contains no data model or merge logic of its own; those live in `reef-lib`, which this crate depends on. It contains no CLI code.

`reef-vcs` depends on this crate and re-exports it. This crate never depends on `reef-vcs`. The design document and roadmap are at <https://github.com/reef-vcs/reef>.
