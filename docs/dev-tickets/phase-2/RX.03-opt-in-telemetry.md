# RX.03 Opt-in telemetry

Cross-cutting · Size M · First needed: Phase 2 · Depends on: R2.05 (the CLI dispatch every command goes through, where invocations are counted), R2.17 (the token-efficiency benchmark whose in-the-wild counterparts this records), R1.04 (configuration), R2.08 (its self-signed certificate helper for the upload test) · Unblocks: R5.06 (fallback telemetry rides on this pipeline)

The brief lives with the Phase 2 tickets because that is where it first runs; it is numbered as a cross-cutting ticket in `docs/dev-phases.md`.

## Context

`dev-phases.md`: "Anonymous usage and performance metrics with a clear opt-in. Needed to measure fallback rates and token savings in the wild." Phase 2 makes the first claims that need numbers from outside the repository: that agents answer repository questions through `reef status` and `reef diff` for a fifth of the tokens (R2.17 measures it on a fixture), that snapshot latency holds on real repositories, that the backends and watchers people actually run are the ones the benchmarks cover. Nothing today tells the project any of that, and nothing should without the user asking.

This ticket ships the client side: what is counted, where it is kept, how a user turns it on, sees exactly what would be sent, and turns it off; and a daemon task that uploads to an endpoint when one is configured. **No collector exists yet.** The endpoint defaults to empty, which means collect locally and upload nowhere; `reef telemetry show` is the whole product until a collector is stood up, and this brief says so in every document it touches. Writing the client first is deliberate: the schema and the privacy rules are what a collector would be built against, and the local report is useful on its own for a user asking why a command is slow.

### What was verified while writing this ticket

Reef at `d156d1e`; crate versions from `cargo info` on 2026-10-25.

- **Where commands are dispatched**: `crates/reef-vcs/src/cli/mod.rs:163-196` is the `Command` enum; `connect()` (`:431-456`) is where every daemon-talking command gets its `Ctx`; `Out` (`cli/out.rs`) owns stdout and stderr, with `note()` for stderr lines that both modes print and `error()` for the exit path; exit codes come from `ErrorKind` (ADR 0025, `reef_daemon::exit_code`). The output mode is `-o`, else `REEF_OUTPUT`, else terminal detection (`:459-470`), so "text mode on a terminal" is a known state at dispatch time.
- **Configuration**: `Config::sources` records which layer (default, file, env, flag) each key came from (`crates/reef-daemon/src/config.rs:717-720`, printed by `config show` as `# source:`), so "the file sets `telemetry.enabled` explicitly" is distinguishable from the default without a tri-state type. Sections are `deny_unknown_fields` structs with `Default` (`:541-575` for `[repo]` and `[store]`, the model); `KEYS` lists every key (`:78-120`).
- **Logging**: `crates/reef-daemon/src/logging.rs` installs the one subscriber; the daemon's JSON log goes to `<state>/logs/daemon.log.<date>` through `tracing-appender`'s non-blocking writer, rotated daily, pruned to `log.keep_days` (`:9-16`). Telemetry is not logging: it must not depend on the log level, must survive `REEF_LOG=off`, and must never contain what the log contains (paths, ids). `reef_lib::telemetry::fields` (`crates/reef-lib/src/telemetry.rs:19-46`) is the list of structured log field names from ADR 0009, several of which (`repo`, `workspace_id`, `path`) are exactly what telemetry must not carry; the name collision is why the telemetry module below is called `usage`.
- **What the daemon already measures**: `SnapshotOutcome { latency_ms, scan_ms?, narrowed, kind, .. }` through the scheduler's `SnapshotHook` (`crates/reef-daemon/src/snapshot/mod.rs:193-251`); `CreateTimings` per create (`proto/.../workspaces.proto:919-941`); the backend and watcher per workspace (`workspaces.backend`, `workspaces.watcher`); lease expiries as `reclaims` rows with `reason`; `Daemon.Status`'s backend probes.
- **HTTP client crates in the tree**: none. `cargo tree -p reef-daemon -e normal` has `hyper` 1.12, `hyper-util` 0.1.21, `axum` 0.8.9 (through tonic) and no `reqwest`, `rustls`, `hyper-rustls`, `ureq`. Versions: `reqwest` 0.13.5, `hyper-rustls` 0.27.10 (features `native-tokio` = `rustls-native-certs`, `http1`, `ring`; default pulls `aws-lc-rs`), `rustls-native-certs` 0.8.5, `ureq` 3.5.0, `webpki-roots` 1.0.11 (**CDLA-Permissive-2.0, not on `deny.toml`'s allow list**). R2.08 adds tonic's `tls-ring` and `tls-native-roots`, so rustls 0.23 with `ring` and native roots will be in the tree when this ticket lands.
- **Install id precedent**: `meta.daemon_instance_id` is a random id written when the database is created (`db/mod.rs:75`); it is per state directory and survives restarts but not a restore (R1.18 sets `restored_from`). It is not reused here (it is tied to the database and to backups); telemetry gets its own id that `off` deletes.
- **R2.17** (its brief) measures tokens an agent spends on three questions through Reef versus git porcelain on a fixture; it reports input and output tokens and tool calls from Claude Code's stream. In the wild Reef never sees the model's tokens, only its own output; bytes of stdout per command are the proxy, and R2.17's results page records the bytes-to-tokens ratio its runs observed so the two can be compared.

## Decisions

- **Off means off.** `[telemetry] enabled = false` by default. While off, nothing is written to disk, no id exists, and the only code that runs is the one-line check. Turning it on is `reef telemetry on`, which writes `<state>/telemetry/consent.json` (`{ "enabled": true, "since": "<rfc3339>", "install_id": "<32 hex>", "version": 1 }`, `0600`, directory `0700`) and prints what will be collected and where it goes (the endpoint, or "nowhere: no endpoint is configured"). `reef telemetry off` deletes the consent file, the install id with it, and every pending event file. The effective state is: if `Config::sources["telemetry.enabled"]` is `File`, the file's value wins (an operator's `false` in `config.toml` disables telemetry for every user of that state directory and `reef telemetry on` says so and refuses; `true` enables it without a consent file, for a fleet an operator owns); otherwise the consent file decides. The daemon and the CLI read the same `usage::Consent::load(&paths)`.
- **What is collected.** Two event kinds, as JSON objects with a fixed, allowlisted shape (`reef_daemon::usage::Event`, serde with `deny_unknown_fields`, every string field an enum or a validated word):
  - `cli`: `{ "kind": "cli", "at": <unix ms, rounded to the minute>, "command": ["ws","create"] (clap's subcommand words only, never values), "flags": ["--purpose","--lease"] (flag names only, sorted, never values), "duration_ms", "exit_kind": "ok" | <ErrorKind name> | "usage", "output": "json" | "text", "remote": bool, "stdout_bytes": <for status, log, diff, ops only; 0 otherwise>, "autostarted": bool, "version", "os", "arch", "install_id" }`.
  - `daemon`: one per hour and at shutdown: `{ "kind": "daemon", "at", "uptime_s", "repos": <count>, "workspaces": { "active": n, "by_backend": {"apfs": n, ...}, "by_watcher": {"notify": n, "watchman": n} }, "snapshots": { "count", "watcher_count", "narrowed_count", "latency_ms": {"p50","p95","max"}, "scan_ms": {"p50","p95"} }, "creates": { "count", "total_ms": {"p50","p95"} }, "leases": { "issued", "expired", "released" }, "errors_by_kind": {"Internal": n, ...}, "listeners": { "web": bool, "remote": bool }, "version", "os", "arch", "install_id" }`.
  Never: paths, repository names or ids, workspace, change, operation, lease or token ids, hostnames, usernames, owner strings, purposes, descriptions, file names, argv values, environment values, error messages, IP addresses. A unit test serializes every `Event` variant with every field populated and asserts no value matches a path, a 16- or 40-hex id, an `@`, or the test machine's hostname and user name. `version`, `os`, `arch` are `CARGO_PKG_VERSION`, `std::env::consts::OS`, `std::env::consts::ARCH`.
- **Where it is kept.** `<state>/telemetry/events-<YYYY-MM-DD>.jsonl`, one object per line, appended with `O_APPEND` by the CLI (which may run concurrently with itself and the daemon; appends under 4 KiB are atomic on both platforms' local filesystems, and lines are far smaller) and by the daemon; files `0600`. Retention: a day's file is deleted once uploaded, or after 7 days, or when the directory exceeds 10 MiB (oldest first); with no endpoint, 7 days is the only rule, so a user who turns telemetry on with no collector has a rolling week of their own usage to look at.
- **`reef telemetry`**: `on`, `off`, `status` (enabled, since, install id, endpoint or "none", pending events and bytes, last upload and its result; JSON and text), `show [--since <date>] [--kind cli|daemon]` (prints the pending events exactly as they would be sent, one per line, so "what does it send" has a literal answer), `upload` (uploads now, for a user who wants to see it work; refused with a hint when no endpoint is configured). Like every command, non-interactive, JSON on a pipe, hints on errors (ADR 0025). `reef telemetry` alone is `status`.
- **The notice.** The first time `reef` runs in text mode on a terminal with no consent file and no `<state>/telemetry/notice-shown` marker, it prints one line to stderr after the command's output: `reef: usage telemetry is off. 'reef telemetry on' shares anonymous counts; 'reef telemetry show' prints exactly what. This notice appears once.` and writes the marker. Never in JSON mode, never on a pipe (agents run on pipes, so no agent sees it), never twice, never when `[telemetry] enabled` is set in the file either way, and never when the command failed (the error is the only thing on stderr then).
- **Upload.** When the endpoint is non-empty and telemetry is on, the daemon runs `usage::uploader` every `[telemetry] upload_interval` (default `24h`, first run 10 minutes after start): reads the pending files oldest first, POSTs each as `application/x-ndjson` (gzip is not used; the files are small) to `[telemetry] endpoint` with `User-Agent: reef/<version>` and no other identifying header, 30 s timeout, and deletes a file on a 2xx; a non-2xx or a network error is logged at `debug` (not `warn`: a failing upload is not the user's problem) and retried next interval. The endpoint must be `https://`; `http://127.0.0.1` and `http://localhost` are allowed for tests only, and anything else is a configuration error at start. The client is `hyper-util`'s legacy client with `hyper-rustls` 0.27 (`http1`, `ring`, `native-tokio`; default features off so no `aws-lc-rs`), reusing the rustls that R2.08 brings in; no `reqwest` (0.13 pulls a second HTTP stack and its own TLS defaults for one POST). Only the daemon uploads; the CLI never makes a network request for telemetry.
- **No collector exists.** The default `endpoint` is `""`. `docs/telemetry.md`, the quickstart's section, `reef telemetry status`, and `reef telemetry on`'s output all say that nothing is uploaded until an endpoint is configured, and `dev-phases.md`'s RX.03 row gains "collector: a follow-up ticket when there is a fleet to measure". When one exists, setting the default is a one-line change plus a CHANGELOG entry; the schema's `version` field is what the collector keys on.
- **R2.17's counterpart.** The `cli` event's `stdout_bytes` for `status`, `log`, `diff` and `ops`, and `duration_ms` for every command, are the in-the-wild numbers R2.17's fixture numbers are compared with; `docs/telemetry.md` names the correspondence and R2.17's results page records its bytes-per-token ratio. Nothing about the agent, model or harness is collected: Reef cannot see them and must not guess.
- **ADR 0040, "Opt-in telemetry"**: off by default with nothing written while off; the consent file and the operator override through `Config::sources`; the allowlisted schema with the privacy rule and its test; local files first, upload second, collector third; `hyper-rustls` over `reqwest`; the notice's rules (once, text mode, terminal, never on failure); alternatives (opt-out, a notice on every run, telemetry in the log file, a hosted analytics SDK, no telemetry); consequences (a schema version to keep, a collector to build, a privacy page to maintain).

## Deliverables

### `crates/reef-daemon/src/usage/{mod,event,consent,store,uploader,summary}.rs`

`Consent` (load, enable, disable, `effective(&Config)`), `Event` and `CliEvent`/`DaemonEvent` with the validators, `Store` (append, pending files, retention, `show`), `Uploader` (the daemon task), `Summary` (the hourly daemon event, fed by a `SnapshotHook`, the manager's create and lease paths, and `status_from`'s error counting by kind). `pub` for the CLI to use through `reef_daemon` behind the `embedded-daemon` feature? No: the CLI must record events without the daemon crate (R2.05's thin-client rule), so `Consent`, `CliEvent`, `Store::append` and `show` live in a small `reef-api`-independent module the CLI can own: `crates/reef-vcs/src/usage.rs` for the CLI's side (consent read, notice, append, `telemetry` command) and `crates/reef-daemon/src/usage/` for the daemon's; the file format and the consent file are the contract between them, documented in both and checked by a shared fixture test. The summary's percentiles use a fixed-bucket histogram, not a sample buffer.

### `crates/reef-daemon/src/config.rs`

`TelemetrySection { enabled: bool = false, endpoint: String = "", upload_interval: Duration = 24h }`, `KEYS` rows, the `https://` validation with the loopback exception, `config show`, the README's config row (`upload_interval` is an added key).

### `crates/reef-vcs/src/cli/{telemetry,mod}.rs`, `crates/reef-vcs/src/usage.rs`

The `telemetry` noun (`on|off|status|show|upload`); the record-on-exit path in `main`'s dispatch (`duration_ms` from process start; `exit_kind` from the `CliError`'s kind; `stdout_bytes` counted by `Out` for the four commands); the notice.

### `crates/reef-daemon/src/daemon.rs`, `snapshot/mod.rs`, `workspace/*.rs`, `lease/mod.rs`, `error.rs`

The uploader task started in the start sequence (step 8b) when enabled with an endpoint; the hourly summary task; the hooks that feed it; the shutdown hook that writes the final summary.

### Tests: `crates/reef-daemon/tests/usage.rs`, `crates/reef-vcs/tests/cli.rs` (additions), unit tests in both `usage` modules

Below.

### `docs/telemetry.md`, `docs/quickstart.md`, `docs/adr/0040-opt-in-telemetry.md`, `docs/adr/README.md`, `CHANGELOG.md`, `docs/dev-phases.md`, `README.md`

`docs/telemetry.md`: what is collected (the two schemas with an example line each), what is never collected, where files live, how to see them, how to turn it on and off, the operator override, that no collector exists and what happens when one does, and R2.17's correspondence. The quickstart gains a "Telemetry" section of six lines pointing there. The README's "What runs today" gains one line. The ADR, its row, the CHANGELOG entry, and the RX.03 row's follow-up note.

## Tests

1. `off_writes_nothing` (`tests/usage.rs`): a daemon and twenty CLI commands with telemetry off: `<state>/telemetry/` does not exist afterwards, and `strace`-free: the test asserts the directory is absent and no `usage` log line exists.
2. `on_off_lifecycle` (CLI): `reef telemetry on -o json` creates `consent.json` (`0600`, directory `0700`) with a 32-hex `install_id` and prints the endpoint state `"none"`; `reef ws list` then appends a `cli` event; `reef telemetry show` prints it as valid JSON with `command == ["ws","list"]`; `reef telemetry off` removes the consent file and every `events-*.jsonl`; `status` says off.
3. `operator_override` (`tests/usage.rs`): `config.toml` with `[telemetry] enabled = false` → `reef telemetry on` is refused (`PermissionDenied`, hint naming the key and file) and nothing is written; with `enabled = true` and no consent file → events are written and `status` says `enabled by config.toml`.
4. `events_carry_no_identifiers` (unit, both modules): every event variant with every field populated through the real recorders (a CLI run with `--purpose "/Users/me/secret"`, `--repo <path>`, an error with a path in its message; a daemon summary after creates and snapshots on a workspace) serializes to text that matches none of: `/`, 16- or 40-hex ids, `@`, the hostname, the user name, the repository's path, the purpose. Argument values are never present; flag names are.
5. `schema_is_closed` (unit): `Event` deserializes the documented example lines from `docs/telemetry.md` (read by the test) and rejects an object with an extra field.
6. `daemon_summary_counts` (`tests/usage.rs`): with the summary interval set to 2 s in the test config (a `cfg(test)`-free knob `[telemetry] summary_interval`, hidden from `KEYS`? No: documented, default `1h`): after two creates, four snapshots and one release, the next `daemon` event has `creates.count == 2`, `snapshots.count >= 4`, `leases.released == 1`, `workspaces.by_backend` naming the backend, and latency percentiles within the test's measured range.
7. `retention` (unit, with a fake clock): files older than 7 days are deleted; the directory is trimmed to 10 MiB oldest first; an uploaded file is deleted at once.
8. `upload_posts_ndjson_and_deletes` (`tests/usage.rs`): a loopback HTTPS server (hyper + rustls with a certificate from R2.08's `generate_self_signed`, trusted through a test-only `[telemetry] ca` key, or `http://127.0.0.1` with the exception) receives one `POST` per pending file with `content-type: application/x-ndjson`, `user-agent: reef/<version>`, and no other header naming the machine; the body's lines parse as `Event`s; files are deleted on 200 and kept on 500 (and retried on the next tick); `reef telemetry upload` triggers it on demand and reports the result.
9. `endpoint_must_be_https` (unit): `http://telemetry.example` is a `ConfigError` at load; `https://...` and `http://127.0.0.1:1234` are accepted.
10. `notice_once_text_mode_only` (CLI): a fresh state dir, `reef ws list` on a pty (or with the test's terminal override) prints the notice once on stderr and writes the marker; a second run does not; `-o json` on a fresh state dir never does; a failing command on a fresh state dir does not; `REEF_OUTPUT=json` does not.
11. `cli_never_opens_a_network_socket` (CLI): with telemetry on and an endpoint configured, `reef ws list` under a `--no-autostart` daemon makes no connection but the Unix socket (checked by running it with the endpoint pointing at a listener that fails the test if contacted).
12. `exit_kind_and_duration` (CLI): a `NotFound` error records `exit_kind == "NotFound"`; `duration_ms` is positive and below the test's bound; `stdout_bytes` is non-zero for `reef status` (R2.02) and zero for `ws list`.

## Implementation notes

- Record the CLI event in `main` after the command returns and after stdout is flushed, in a `Drop`-safe path so a panic still records nothing rather than something partial; never let a telemetry failure change the exit code or print anything (a failed append is `debug`-logged only).
- Command words come from clap's matched subcommand path (`ArgMatches::subcommand()` chain), never from `std::env::args()`, so values and typos cannot leak; flag names from the matched arguments' ids with their `--` form.
- `stdout_bytes` is counted in `Out` where bytes are written, for the four commands only; everything else reports 0 so the field is never a proxy for content size elsewhere.
- The daemon's error counting hooks `status_from` (`error.rs:236`), which already sees every error's kind; count there, never the message.
- Percentiles: a 64-bucket log-scale histogram per metric, reset each hour; `p50`/`p95`/`max` from it. No per-event storage.
- The uploader shares the runtime's rustls provider with R2.08; if this ticket lands first, add `hyper-rustls` with `ring` and `native-tokio` and no default features, and check `cargo deny` for the license set (`aws-lc-rs` must not appear).
- `reef telemetry show` reads the files with the same `Event` type the writers use, so a line the schema rejects is reported as such rather than hidden; that is also the test of forward compatibility when the schema grows (`version` bumps, old lines still parse).
- Keep `docs/telemetry.md`'s example lines real: generate them in a test from the recorders and compare, so the document and the code cannot drift.

## Acceptance criteria

- [ ] Tests 1 to 12 pass on both runners.
- [ ] With telemetry off (the default), a full `just ci` run and the quickstart's commands leave no `<state>/telemetry/` directory and no `install_id` anywhere on disk.
- [ ] `reef telemetry show` output, pasted into `docs/telemetry.md`, is what test 5 parses; the document lists every field of both schemas and the never-collected list.
- [ ] Every new config key is in `KEYS`, `config show`, and the README's config row; the `https://` rule holds.
- [ ] `cargo deny check licenses` passes with the HTTP client added (no `webpki-roots`, no `aws-lc-rs`).
- [ ] ADR 0040 written and indexed; quickstart, README, CHANGELOG and `dev-phases.md` updated, each saying that no collector exists and nothing is uploaded by default.
- [ ] `just ci` passes.

## Verification

```sh
reef telemetry status                       # off, nothing on disk
ls "$(reef config show -o json | jq -r .paths.state_dir)/telemetry" 2>&1   # No such file
reef telemetry on && reef ws list >/dev/null && reef telemetry show | jq .
reef telemetry off && reef telemetry status
cargo nextest run -p reef-daemon --test usage && cargo nextest run -p reef-vcs --test cli -E 'test(/telemetry|notice/)'
just ci && cargo deny check licenses
```

## Out of scope

- The collector, its storage, dashboards, and the public results page (a follow-up ticket when there is a fleet to measure; `docs/telemetry.md` says so).
- Crash reports, stack traces, or anything from the log file.
- Per-repository or per-user opt-in; the unit is the state directory.
- Telemetry from the SDKs or the MCP server beyond what the daemon sees (they are clients; their calls show up in the daemon's counts as RPCs, not as events, in this phase).
- Measuring model tokens; Reef cannot see them.

## Risks

- **Trust.** A VCS that phones home is a VCS people stop using. Off by default, nothing written while off, a literal `show`, and no collector yet are the answer; the review should read the privacy test (test 4) as the most important one in the ticket.
- **The schema will want to grow** (R5.06's fallback counts, R3's landing counts). The `version` field, the closed schema, and `show`'s "line rejected" behavior make growth visible; the ADR says new fields go through the same never-collected test.
- **Two writers to one file.** The CLI and the daemon append to the same day's file; appends are small and `O_APPEND`, and a torn line is tolerated by `show` and the uploader (skipped and counted). A per-process file would avoid even that at the cost of many small files; decided against, recorded in the ADR.
- **A collector that never arrives** leaves `upload` dead code. It is small, tested against a loopback server, and the local report stands on its own.

## Proposed departure from the README

*Accepted 2026-10-25: the README's configuration row lists `upload_interval`, `summary_interval` and `ca`.*

None of substance. Added keys beyond the README's row: `[telemetry] upload_interval = "24h"` and `summary_interval = "1h"`; the README's config section should list them. The CLI's side of telemetry lives in `reef-vcs` (not `reef-daemon`) so R2.05's thin-client rule holds; the README's "Crates" rows are unaffected.
