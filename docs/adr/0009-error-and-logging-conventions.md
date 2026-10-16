# 0009. Error and logging conventions

- Status: accepted
- Date: 2026-10-16
- Ticket: R0.09

## Context

The design calls for structured responses everywhere: every API call returns data, including the operation it created, and agents act on what comes back without reading prose. Errors that cross the API are therefore data too, and the daemon has to turn any of them into a gRPC status code (R1.05) and the CLI into an exit code (R1.22) without matching on message text.

The daemon is long-running and its logs are read by agents and tools more often than by people, so events must be machine-readable and their field names stable. R0.04 already wrote the first error enums in `reef-lib`. Settling the shape now, while there is one module, is cheap; after Phase 1 it would be a rewrite.

## Decision

**Errors.** Libraries define `thiserror` 2 enums, one per module or operation family, named `<Thing>Error` and marked `#[non_exhaustive]`, with causes attached through `#[source]` or `#[from]`. There is no `Box<dyn Error>` and no `anyhow` in `reef-lib` or in `reef-daemon`'s library code. Variants carry the identifiers a caller needs to act (`path`, `workspace_id`, `op_id`), never pre-formatted messages. `Display` text is lowercase with no trailing period and no "error:" prefix, and does not repeat its source's message, so a chain reads as one sentence. Binaries use `anyhow` in `main.rs` and nowhere else; `main` returns `anyhow::Result<()>`, so a failure prints the whole chain and exits 1. Test code may use `anyhow`, `unwrap`, and `expect`. In library code, `unwrap` and `expect` need a comment proving the invariant, and expected conditions never panic.

**Classification.** `reef_lib::error` defines `ErrorKind` (`NotFound`, `AlreadyExists`, `InvalidArgument`, `Conflict`, `Unavailable`, `PermissionDenied`, `Io`, `Internal`; `#[non_exhaustive]`) and the trait `Classify { fn kind(&self) -> ErrorKind }`. Every error type that crosses a crate boundary implements it, with an exhaustive `match`, so adding a variant forces a decision about its kind. *(addition)* `Conflict` also covers failed preconditions, such as a stale working copy or undoing the root operation, since the set has no separate kind for them. *(addition)* jj-lib's errors do not say whose fault a failure is. Except for the few variants Reef can name (for example "no workspace here" is `NotFound`), they are classified by `classify_by_source`: the first `std::io::Error` in the source chain gives `PermissionDenied` or `Io`, and no I/O error gives `Internal`. An I/O `NotFound` deep inside jj means a repository file is missing, not that the caller named something absent, so it maps to `Io`.

**Events.** All diagnostics go through `tracing`, called by full path (`tracing::info!`). There is no `log` crate, and no `println!`/`eprintln!` outside `main.rs` and CLI output paths. The message is a short lowercase phrase, and the data goes in fields whose names come from `reef_lib::telemetry::fields`: `repo`, `workspace_id`, `change_id`, `op_id`, `commit_id`, `agent_id`, `lease_id`, `backend`, `path`, `elapsed_ms`, `error`, or any name ending in `_count`, `_ms`, or `_bytes`. A unit test in `reef-lib` scans its sources and fails on any other name. Levels:

- `error`: the daemon could not do what it was asked and someone must look.
- `warn`: degraded but handled (a fallback backend, a retry, files left untracked) or, as below, a failure the caller can fix.
- `info`: state transitions (snapshot taken, operation undone).
- `debug`: per-step detail.
- `trace`: per-file or per-request payloads.

The default filter is `info`. Secrets never appear in fields, and types holding them redact their `Debug` output.

**Spans and failures.** Every public async entry point in `reef-lib`, and every RPC handler later, opens a span with `#[tracing::instrument(skip_all, fields(..))]` carrying its identifying fields. *(addition)* It reports a returned error exactly once, through `reef_lib::telemetry::log_failure`, which records the error and its whole source chain in the `error` field. The level is `warn` when `ErrorKind::is_caller_error` is true (`NotFound`, `AlreadyExists`, `InvalidArgument`, `Conflict`, `PermissionDenied`) and `error` otherwise. Code beneath the entry point returns errors without logging them. An entry point that calls another one as a step calls its unlogged form (`snapshot_unlogged` inside `undo_last`), so a failure is not reported twice. `#[instrument(err)]` was not used because it records only the top error's `Display` (`error = %e`), and the JSON layer does not walk `source()`. The cause, usually the useful part, would be lost.

**Subscriber.** `reef_daemon::logging` installs the one global subscriber for both binaries; libraries only emit. Logs go to stderr. `REEF_LOG` holds an `EnvFilter` directive, falling back to `RUST_LOG` and then to `info`. `REEF_LOG_FORMAT` is `auto`, `pretty`, or `json`, where `auto` means pretty when stderr is a terminal and JSON otherwise. An empty variable counts as unset, and an invalid value is an error rather than silently ignored. *(addition)* For that reason `LogOptions::from_env` returns a `Result`. Pretty output is one line per event, coloured only on a terminal and never when `NO_COLOR` is set. JSON output is `tracing-subscriber`'s JSON layer with `flatten_event(true)`, `with_current_span(true)`, `with_span_list(false)`, RFC 3339 UTC timestamps, and `target`, so event fields are top-level keys next to `timestamp`, `level`, `message`, `target`, and `span`. This is the format of the daemon's log file in R1.03.

This ADR does not cover mapping `ErrorKind` to `tonic::Status` (R1.05) or exit codes (R1.22), log files and rotation (R1.03), OpenTelemetry export (RX.03), or redaction machinery. `reef-testing` is unpublished dev tooling and is exempt from `Classify`.

## Consequences

- Every new error enum needs a `Classify` impl. This is a few lines, and the exhaustive match keeps it current.
- No free-form error strings means adding a variant when a new failure appears. That is the point: callers can match on it, and R1.05 can map it.
- Each logged entry point is a thin wrapper around an unlogged body. Entry points that compose others call the unlogged body.
- JSON is the default whenever stderr is not a terminal, so a person reading piped or redirected output sets `REEF_LOG_FORMAT=pretty`.
- The field-name test is a regex-level scan, not a parser. Events must be written as `tracing::<level>!` for it to see them, and it covers only `reef-lib`. Binaries may log other fields, such as `version` on start-up.
- We would revisit `log_failure` if `tracing` gains a way for `#[instrument(err)]` to record a full error chain in every layer.

## Alternatives considered

**`anyhow` everywhere.** Simplest to write, but errors become opaque, and mapping them to status codes would mean downcasting or string matching. Rejected.

**`snafu`.** Equally capable, with context selectors. `thiserror` is what jj-lib and most of the ecosystem use, so Reef's errors read like their sources. Rejected for consistency.

**`log` with `env_logger`.** No spans and no structured fields, both of which the daemon needs for per-operation context. Rejected.

**`slog`.** Structured, but less maintained and less widely integrated than `tracing`, which tokio, tonic, and jj-lib already use. Rejected.

**`#[instrument(err)]` for failure logging.** Rejected for the reason above: it drops the source chain.
