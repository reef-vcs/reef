# reef-testing

Test fixtures and dev tooling for **Reef**: the synthetic repository generator, fixture builders, and test helpers. It is not published to crates.io.

It must not depend on any other Reef crate, so that `reef-lib` and the others can take it as a dev-dependency without a cycle. Its `tests/crate_graph.rs` enforces the workspace's dependency direction.

Only dev-dependencies and benchmarks reach this crate. It is excluded from the workspace's `default-members`, so build it with `--workspace` or `-p reef-testing`.
