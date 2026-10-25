# reef-api

The daemon API of **Reef**, version control built for a world where AI agents write most of the code: the `reef.daemon.v1` protobuf schema in `proto/`, the Rust code generated from it at build time (without `protoc`), the Rust client, the JSON form of its responses, and the error kinds every Reef crate classifies its errors by.

It is the bottom crate. `reef-lib`, `reef-daemon`, and `reef-vcs` depend on it; it depends on no other Reef crate and never on jj-lib, so SDKs, the MCP server, and other tools can reach a daemon without linking one. The API is described for client authors in [`docs/api.md`](../../docs/api.md), and its design in [ADR 0029](../../docs/adr/0029-daemon-api-v1-and-the-reef-api-crate.md). The design document and roadmap are at <https://github.com/reef-vcs/reef>.
