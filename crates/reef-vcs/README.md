# reef-vcs

**Reef** is version control built for a world where AI agents write most of the code. This crate builds the `reef` binary, the command-line client (and later `reef mcp`, the MCP server), and re-exports `reef-api` and `reef-client` so a Rust program that talks to the daemon can depend on one crate.

The CLI is a thin client of the daemon ([ADR 0033](../../docs/adr/0033-thin-client.md)). It holds no data model, merge, or storage logic of its own, and links no daemon code and no jj-lib: it depends on `reef-client`, `reef-api` and `reef-error` alone, which a test over the dependency graph enforces. `reef daemon run`, `reef config show` and `reef restore` run the `reef-daemon` binary, beside `reef` or on `PATH`, so install both (`cargo install --git https://github.com/reef-vcs/reef reef-vcs reef-daemon`).

Nothing in the workspace depends on this crate; it sits at the top of the dependency graph. The design document and roadmap are at <https://github.com/reef-vcs/reef>.
