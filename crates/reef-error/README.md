# reef-error

The error conventions of **Reef**, version control built for a world where AI agents write most of the code ([ADR 0009](../../docs/adr/0009-error-and-logging-conventions.md)): `ErrorKind`, the coarse classification every Reef error has, which is a gRPC status code at the API and an exit code in the CLI; `Classify`, which every error type that crosses a crate boundary implements; `ErrorChain`, an error and its sources on one line; and `Advice`, the `hint` and `next` of [ADR 0025](../../docs/adr/0025-agent-facing-cli-contract.md).

It depends on nothing, so every other Reef crate can depend on it: `reef-lib` re-exports it as `reef_lib::error`, and the thin client (`reef-client`) and the CLI (`reef-vcs`) use it without linking jj-lib ([ADR 0033](../../docs/adr/0033-thin-client.md)). The design document and roadmap are at <https://github.com/reef-vcs/reef>.
