# reef-lib

The core library of **Reef**, version control built for a world where AI agents write most of the code. It holds the data model, the embedding of [jj-lib](https://crates.io/crates/jj-lib), the workspace model, the merge engine, and the query layer.

It contains no server code and no CLI code. Everything here must be usable by a process that is not the daemon: tests, benchmarks, and other tooling.

`reef-daemon` depends on this crate; the `reef` binary (`reef-vcs`) does not ([ADR 0033](../../docs/adr/0033-thin-client.md)). Its error conventions, `reef_lib::error`, are `reef-error`'s, re-exported. This crate depends on neither. The design document and roadmap are at <https://github.com/reef-vcs/reef>.
