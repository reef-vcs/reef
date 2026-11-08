# reef-executor

Reef's reference executor (R3.06, [ADR 0060](../../docs/adr/0060-reference-executor.md)), behind `reef executor`. It claims jobs from a Reef daemon over [executor protocol 1](../../docs/executor-protocol.md), reads and checks each job's tree, runs the declared checks in a sandbox, and returns signed evidence. [docs/executor.md](../../docs/executor.md) is the operator's guide.

A client of the daemon like `reef-vcs`: it reaches the daemon only through `reef-client`, and links no jj-lib, daemon or SQLite (`reef-testing`'s `crate_graph` holds it to that).

| Module | |
| --- | --- |
| `config` | `executor.toml` and `reef-checks.toml`, parsed strictly; checks, targets as argv data |
| `declare` | each requested pair matched to checks, the executor's first |
| `githash` | git blob and tree ids with collision-detecting SHA-1, from entries or from a directory |
| `materialize` | the tree through `FileService`, a verified blob cache, the layout, and the three checks |
| `sandbox` | `Policy` and `Backend`: Seatbelt profiles, bubblewrap arguments, or none; no job knowledge, so `reef ws run --sandbox` (#62) can use it |
| `run` | one check as a process: its session, its capped log, `wait4`'s CPU time, kills and the sweep |
| `result` | exit statuses to results, summaries and finer objects, annotations |
| `job` | one job end to end: the key, the tree, the plan, the checks, heartbeats, the completion |
| `serve` | the claim loop, the work directory's lock, signals, process hardening |
| `probe` | the escape probes `reef executor check` runs |
| `report` | the JSON `reef executor run` and `check` print |

The sandbox's guarantees and limits per platform are in [docs/executor.md](../../docs/executor.md#the-sandbox), and its findings are rows S-82 to S-89 of the [threat model](../../docs/threat-model.md). Its tests need the platform's sandbox to apply (`sandbox-exec` outside any other sandbox on macOS; `bwrap` with user namespaces on Linux); where it does not, they run checks unsandboxed and say so, and `REEF_EXECUTOR_TESTS_REQUIRE_SANDBOX=1` makes that a failure.
