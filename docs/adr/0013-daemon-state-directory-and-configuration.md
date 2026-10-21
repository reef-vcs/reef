# 0013. Daemon state directory and configuration

- Status: accepted
- Date: 2026-10-16
- Ticket: R1.04

## Context

One daemon runs per machine and serves many repositories ([0012](0012-daemon-process-model-and-threading.md)). It owns a metadata database, a socket, a lock, logs, base cache entries, workspace roots, jj working-copy state for each workspace, and later shared dependency stores. A backup (R1.18) has to copy all of it and nothing else, and a user who wants a clean slate should be able to delete one directory.

The daemon's settings (lease lengths, watcher pacing, the workspace backend, GC and backup windows, the identity on the commits it writes) are set by whoever runs the daemon, not by the agents that call it. Agents never read configuration; they get the values that matter (such as how often to heartbeat) in API responses.

jj-lib builds its behavior from a `UserSettings` value. jj-cli fills it from the user's jj config files. jj-lib's own defaults leave `operation.hostname` and `operation.username` empty ([0008](0008-jj-lib-embedding-findings.md)), so operations written without them do not say where they came from.

## Decision

We will keep everything the daemon owns under one state directory, configured by one TOML file, and never read jj's user configuration.

- **State directory.** `REEF_STATE_DIR`, else `$XDG_STATE_HOME/reef` (default `~/.local/state/reef`) on Linux and `~/Library/Application Support/reef` on macOS, created with mode `0700`. Layout: `config.toml`, `daemon.sock`, `daemon.lock`, `daemon.pid`, `reef.sqlite3` (with `-wal` and `-shm`), `logs/`, `repos/<repo-id>/bases/<tree-id>/` (with `<tree-id>.state/` beside each), `repos/<repo-id>/ws/<ws-id>/` (overlay backends add `<ws-id>.upper/` and `<ws-id>.work/`), `repos/<repo-id>/wc/<ws-id>/`, and `stores/<name>/<lockfile-hash>/`. Backups go wherever the caller says, never inside it.
- **Socket.** `REEF_SOCKET`, else `daemon.socket`, else `$XDG_RUNTIME_DIR/reef/daemon.sock` on Linux for the default state directory only, else `<state>/daemon.sock`. The path must fit `sockaddr_un` (103 bytes plus the NUL, macOS's limit, on both platforms).
- **Config file.** `REEF_CONFIG`, else `<state>/config.toml`; a missing `<state>/config.toml` means defaults, while a missing file named by `REEF_CONFIG` or `--config` is an error. Sections `[daemon]`, `[identity]`, `[log]`, `[lease]`, `[watcher]`, `[workspace]`, `[base_cache]`, `[gc]`, `[backup]`, `[snapshot]`, `[repo]` (R1.25), with the keys and defaults in the Phase 1 ticket README. Unknown keys are errors, so a typo cannot silently leave a setting at its default. Durations are humantime strings (`"500ms"`, `"30m"`, `"7d"`), printed back in the largest exact unit; sizes are bytes or a string with `KiB`, `MiB`, or `GiB`; relative paths are relative to the state directory. Values are validated on load (for example `lease.default <= lease.max`, `lease.tick < lease.grace`).
- **Precedence.** Built-in default, then the file, then the environment, then command-line flags. The only environment variables are `REEF_STATE_DIR`, `REEF_SOCKET`, `REEF_CONFIG`, and R0.09's `REEF_LOG`, `RUST_LOG`, and `REEF_LOG_FORMAT`. `reef config show` prints the effective configuration as TOML that loads back as a `config.toml`, with the layer each key came from as a comment, and works without a daemon.
- **Identity.** Commits are written as `[identity] name` (default `reef`) and `email` (default `reef@<hostname>`, the full hostname in lowercase). Every operation records the machine's hostname as `operation.hostname` and the OS user as `operation.username`; neither is configurable in Phase 1.
- **jj settings.** Built from jj-lib's bundled defaults plus a layer generated from `[identity]`, `[snapshot]`, and the operation identity. jj's own user config files are never read.
- **Errors.** A malformed file, an unknown key, or a value that breaks a rule is an `InvalidArgument` error naming the key and the file (with toml's line and caret for parse errors), and both `reef` and `reef-daemon` exit 2 on it.

## Consequences

- Everything the daemon owns is under one directory: R1.18 copies it, and deleting it resets Reef on that machine without touching any repository.
- The state directory's path length counts against the socket limit; a deeply nested `REEF_STATE_DIR` needs `REEF_SOCKET` set, and the daemon says so instead of failing at bind time.
- A user's jj configuration (their name, aliases, ignore settings, signing) never changes what the daemon writes or snapshots. A user who wants Reef's commits under their own name sets `[identity]`.
- Operations the daemon writes say which machine and user wrote them, which `jj op log` shows and later sync (Phase 4) relies on.
- Adding a key or section is a schema change, reviewed like an API change: unknown keys are errors, so a file written for a newer daemon fails on an older one with a message naming the key.
- Configuration is read once at start; changing it means restarting the daemon.

## Alternatives considered

**Read jj's user configuration.** It would pick up the user's identity for free. Rejected: it couples Reef to the identity, ignore rules, and settings of whichever user's jj config the daemon happens to see, lets a personal setting change what agents' snapshots contain, and makes the daemon's behavior depend on files it does not own.

**A generic `REEF_<SECTION>_<KEY>` environment mapping.** Every setting overridable without a file. Rejected: the variables cannot be discovered from `--help` or the file, a typo in a variable name is silently ignored, and the daemon is started rarely enough that a file is not a burden.

**Per-repository configuration in `.jj`.** Settings next to the repository they apply to. Rejected: the repository is the user's, not Reef's, and the daemon's settings are per machine. Per-repository settings that belong to the project (lease length, snapshot excludes, stores) live in the committed `reef.toml` (R1.19).

**Unknown keys as warnings.** Friendlier when downgrading. Rejected: a misspelled key that only warns is a setting that silently does not apply, and the daemon's log is the last place someone looks.
