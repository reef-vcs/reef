# reef-lib

The core library of **Reef**, version control built for a world where AI agents write most of the code. It holds the data model, the embedding of [jj-lib](https://crates.io/crates/jj-lib), the workspace model, the merge engine, and the query layer.

It contains no server code and no CLI code. Everything here must be usable by a process that is not the daemon: tests, benchmarks, and other tooling.

`reef-daemon` depends on this crate, and `reef-vcs` re-exports it. This crate depends on neither. The design document and roadmap are at <https://github.com/reef-vcs/reef>.
