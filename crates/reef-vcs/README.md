# reef-vcs

**Reef** is version control built for a world where AI agents write most of the code. This crate builds the `reef` binary, the command-line client (and later `reef mcp`, the MCP server), and re-exports `reef-lib` and `reef-daemon` so a downstream Rust user can depend on one crate.

The CLI is a thin client of the daemon. It holds no data model, merge, or storage logic of its own.

Nothing in the workspace depends on this crate; it sits at the top of the dependency graph, above `reef-daemon` and `reef-lib`. The design document and roadmap are at <https://github.com/reef-vcs/reef>.
