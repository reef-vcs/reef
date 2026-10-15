# Phase 0 tickets

Source of truth for scope: [`docs/dev-phases.md`](../../dev-phases.md) (Phase 0 table) and [`docs/design.md`](../../design.md). These files expand each Phase 0 row into an implementation brief. They are written for an implementing agent, not for humans: every decision that could be made in advance has been made here, and the brief says so. If a brief and `dev-phases.md` disagree on scope, the brief wins for the implementation and the disagreement should be fixed in `dev-phases.md` in the same PR.

## Tickets

| ID | File | Size | Depends on | Unblocks |
| --- | --- | --- | --- | --- |
| R0.01 | [R0.01-workspace-layout.md](R0.01-workspace-layout.md) | S | nothing | R0.02, R0.04, R0.05, R0.09 |
| R0.02 | [R0.02-toolchain-and-dev-tooling.md](R0.02-toolchain-and-dev-tooling.md) | S | R0.01 | R0.03 |
| R0.03 | [R0.03-ci.md](R0.03-ci.md) | S | R0.02 | R0.06 (artifact upload) |
| R0.04 | [R0.04-jj-lib-spike.md](R0.04-jj-lib-spike.md) | M | R0.01 | R0.06, R1.01, R1.11 |
| R0.05 | [R0.05-synthetic-repo-generator.md](R0.05-synthetic-repo-generator.md) | S | R0.01 | R0.06, R1.10 |
| R0.06 | [R0.06-benchmark-harness.md](R0.06-benchmark-harness.md) | S | R0.04, R0.05 | R1.10, RX.06 |
| R0.07 | [R0.07-adr-directory.md](R0.07-adr-directory.md) | S | nothing | ADRs written by R0.01, R0.04, R0.08, R0.09 |
| R0.08 | [R0.08-contributor-docs.md](R0.08-contributor-docs.md) | S | nothing (R0.02 for accuracy of the setup section) | |
| R0.09 | [R0.09-error-and-logging-conventions.md](R0.09-error-and-logging-conventions.md) | S | R0.01 | R1.03 |

## Suggested execution order

1. R0.07 and R0.01 in parallel. R0.07 first if only one agent, because R0.01 writes an ADR using its template.
2. R0.02, then R0.03.
3. R0.04, R0.05, R0.09 in parallel (all only need R0.01). R0.08 any time.
4. R0.06 last.

## Phase exit criteria (from dev-phases.md)

- CI runs build, test, clippy, and rustfmt on macOS and Linux on every push.
- A spike proves the daemon can open an existing jj repository through jj-lib, create a snapshot, and read the operation log.
- A synthetic repository generator produces a 100k-file tree for benchmarks.

## Decisions fixed across all Phase 0 tickets

These were settled while writing the briefs. Do not re-open them inside a ticket; open a new ADR if one turns out to be wrong.

| Topic | Decision | Set by |
| --- | --- | --- |
| Edition | 2024 | R0.01 |
| Pinned toolchain | `1.99.1` (current stable on 2026-10-13) in `rust-toolchain.toml` | R0.02 |
| MSRV (`rust-version`) | `1.97.1`, the floor jj-lib 0.46.0 imposes. The local machine had 1.96.0 when these were written; `rustup update` is required. | R0.02 |
| jj-lib version | `=0.46.0` (exact pin; jj-lib is 0.x and breaks between minors). `jj-lib` 0.46 re-exports many modules from a new `jj-core` crate (`matchers`, `op_store`, `repo_path`, `backend`, ...); the paths `jj_lib::matchers` etc. still work. | R0.04 |
| Crates | `reef-lib` (library), `reef-daemon` (library + `reef-daemon` binary), `reef-vcs` (umbrella library + `reef` binary), `reef-testing` (unpublished dev tooling: synthetic repo generator, fixtures) | R0.01 |
| Dependency direction | `reef-vcs` → `reef-daemon` → `reef-lib`. `reef-testing` depends on none of them. `reef-lib` may dev-depend on `reef-testing`. | R0.01 |
| Task runner | `just` (`justfile` at the repo root). CI runs the same recipes. | R0.02 |
| Async runtime | tokio in binaries. jj-lib's async functions are runtime-agnostic; tests may use `#[tokio::test]` or `pollster`. | R0.04 |
| Errors | `thiserror` 2 enums in libraries, `anyhow` only in `main.rs` files and test code | R0.09 |
| Logging | `tracing` + `tracing-subscriber`; `REEF_LOG` filter env var; JSON when stderr is not a terminal | R0.09 |
| Benchmarks | criterion 0.8, benches live in `crates/reef-lib/benches/` | R0.06 |
| ADR numbering | Four digits, assigned in this folder so parallel tickets do not collide: 0001 ADR process, 0002 jj-lib, 0003 SQLite, 0004 gRPC/tonic, 0005 Tauri, 0006 CEL, 0007 crate layout, 0008 jj-lib embedding findings, 0009 errors and logging, 0010 DCO | R0.07 and others |
| Commit style | Imperative sentence-case subject, no trailing period, no conventional-commit prefix, body explains why. Matches existing history. | R0.08 |
| Contributor license | DCO sign-off, not a CLA | R0.08 |

## Ticket file format

Each ticket has: Context, Decisions, Deliverables (file by file), Implementation notes, Acceptance criteria, Verification, Out of scope, Risks. Acceptance criteria are written so an agent can check each one mechanically before opening the PR.
