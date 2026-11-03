# Issue #188 A create waits for the first copy

Phase 2 · Size S (no row; see "Size") · Fixes: issue #188 · Depends on: R2.05 (the CLI over the v1 API, `Ctx::call_with_repo_within`), R2.13 (the MCP `create_workspace` tool), R2.26 (the provider's `CREATE_DEADLINE`) · Unblocks: none listed · Lands before the 0.4.0 tag

## Context

The first create in a repository checks the whole tree out into the base cache. On a 277k-file git monorepo (2.1 GB checkout) that took 58 s. `reef ws run` and `reef ws create` give every RPC 30 s, so the CLI quit with a deadline error while the daemon was still checking the tree out. The checkout finished anyway, and the next create took 2.2 s. Register has its own 10-minute default and the workspace provider gives a create 300 s, but `ws create`, `ws run` and the MCP `create_workspace` tool use the 30 s default.

This ticket gives a workspace create a default deadline of 5 minutes, one constant shared with the provider. It gives a create's deadline error its own message and hint: the first create builds the base cache, the build goes on after the CLI quits, a retry waits for that build, and `--timeout` waits longer. `ws create` and `ws run` also print one stderr note when a create has run 5 s. The change is in the CLI only. There is no API, schema or daemon change. Following create progress over presence goes to a follow-up issue (see "Progress over presence: a follow-up").

### What was verified while writing this ticket

All of this was checked at d8ed637 (main), with the installed `reef` 0.3.0 (ba17f1f). Paths are under `crates/` unless they start with `docs/` or `sdk/`.

- **How `--timeout` works today.** `--timeout` is a global flag, hidden from `--help` and shown in `--help-all` (`reef-vcs/src/cli/mod.rs:179-182`, parsed by `humantime`). `connect` sets `Ctx.timeout` to the flag or `DEFAULT_TIMEOUT` (30 s, `mod.rs:70-71`), and `Ctx.timeout_flag` to the flag as given (`mod.rs:760-761`, `:794-795`). `Ctx::call` and `Ctx::call_within` (`mod.rs:384-402`) wrap the call in `tokio::time::timeout`. When it fires, the call's future is dropped and `CliError::Timeout { after }` is returned. The client sends no `grpc-timeout` header (`reef-client` never sets one), and the daemon has no deadline of its own. Commands with their own default use the flag or that default: register 10 min (`REGISTER_TIMEOUT`, `mod.rs:75`, used at `:451`, `init.rs:240`, `repo.rs:63`), `ws gc` 1 h (`ws.rs:673-679`), `store build` 30 min plus 10 min of slack (`store.rs:18-24`, `:107-109`), and `backup` 1 h (`backup.rs:19`, `:49`). A few take the longer of `--timeout` and a minimum instead: the provider's create and release (`provider.rs:468`, `:800`, 300 s), `ws run`'s final release (`ws_run.rs:476`, `FINISH_DEADLINE` 600 s), and `symbols` (`symbols.rs:248`).
- **The create call sites.**
  - `ws create`: `ctx.call_with_repo(&args.target.repo()?, |repo| ctx.client.create_workspace(…))` (`ws.rs:727-744`), so 30 s.
  - `ws run`: the same (`ws_run.rs:277-301`), so 30 s. It is called from `run` (`ws_run.rs:179`) before the command starts.
  - `ws provider create`: `ctx.call_with_repo_within(ctx.timeout.max(CREATE_DEADLINE), …)` (`provider.rs:46-48`, `:468`, `:503-509`), so 300 s.
  - MCP `create_workspace`: `ctx.call(ctx.client.create_workspace(…))` (`reef-vcs/src/mcp/tools.rs:1007-1034`). The MCP server's `Ctx` is the CLI's, so 30 s, or `reef mcp --timeout`. Its errors go through `error_object(&CliError)` (`mcp/mod.rs:62-86`), so they carry the same hint.
  - `call_with_repo_within` (`mod.rs:423-445`) applies the deadline to each attempt. If the first attempt fails as `REPOSITORY_NOT_REGISTERED`, it registers with `REGISTER_TIMEOUT` (`register_implicitly`, `mod.rs:448-470`) and calls again with the same deadline. So a first `ws run` in an unregistered checkout gets 10 min to register, then 30 s to create.
- **The SDKs do not have this problem.** Python's `reef.connect(timeout=None)` sets no deadline by default (`sdk/python/src/reef/client.py:392`, `:413`). `client.workspace()` creates with the client's deadline (`sdk/python/src/reef/workspace.py:239`). TypeScript's `ConnectOptions.timeoutMs` is "none by default" (`sdk/typescript/src/options.ts:36-37`), and `openWorkspace` passes no per-call deadline (`sdk/typescript/src/workspace.ts:348-370`). Only heartbeats and presence reports set deadlines there (5 s). A caller that sets a client-wide deadline gets that deadline, which is the caller's choice. No SDK change is needed.
- **Today's error**, from `reef --timeout 1ms ws list --limit 1 | cat` (exit 6):

  ```
  {"error":{"kind":"unavailable","message":"the daemon did not answer within 1ms (--timeout); it may still finish the request","hint":"Retry with a longer --timeout; the request may have finished anyway, so check (reef ws list) before repeating a create."}}
  ```

  The message is `CliError::Timeout`'s (`reef-vcs/src/cli/error.rs:93-101`) and the hint is its advice (`error.rs:221-223`). It has no `next`. The kind is `Unavailable` (`error.rs:265-267`), so the exit code is 6. `stream.rs:89` treats a `Timeout` as a reconnectable stream failure.
- **What a create does after its client gives up.** Dropping the call resets its HTTP/2 stream. hyper's server polls for a reset while the handler's future is pending and drops the future when one comes (`hyper-1.12.0/src/proto/h2/server.rs:451-460`). Tonic and Reef's layers (`reef-daemon/src/server.rs:151-179`) spawn nothing in between. So `Manager::create` (`reef-daemon/src/workspace/create.rs:117`) stops at its next `.await`. Work it handed to another thread runs to completion and its result is thrown away. By step:
  - **Steps 1-2** (resolve, ref import on the repository thread): the import's operation is recorded. Nothing else is left.
  - **Step 3** (the base, `create.rs:175`, `ensure_base_of` at `:420-489`): the checkout runs in `spawn_blocking` (`:432`) and finishes. The base cache entry is sealed and recorded. **This is what the issue saw.** The base cache's per-tree lock makes concurrent calls "wait for each other and materialize once" (`reef-daemon/src/backend/base_cache.rs:240-243`). So a retry during the build waits for it, and a retry after it gets `base_was_cached: true`.
  - **Steps 4-6** (materialize, mount, jj, seed; `create.rs:181-210`): `Undo` has no `Drop` (`create.rs:839-870`), so a drop here leaves the directory under `ws/<id>`, maybe a jj workspace `reef-<id>`, and a state directory, with no row. `sweep_abandoned_creates` removes them, but only at the next daemon start (`workspace/abandoned.rs:1-10`, `:48`; called from `daemon.rs:552`).
  - **Step 7 onward** (`create.rs:238-252`, `load_created` at `:332-333`): the row and the lease are written synchronously. A drop in `scheduler.track(…).await` leaves an active, leased workspace that nobody holds, and its watch may not be set up. Its lease runs out and the daemon reclaims it: 30 min plus 5 min of grace by default (`reef-daemon/src/config.rs:293`; `docs/workspace-provider.md:76`).
  - The create's presence entry is a `Progress` guard, "ended when dropped, whatever the outcome" (`create.rs:146-157`). It ends `CLEARED` when the handler is dropped.

  The issue's case is step 3, which leaves nothing behind but a finished base. The step 4-7 cases need the deadline to fire in a window of a second or two after a base that took most of the deadline. That is rare at 30 s and rarer at 5 min. It is a follow-up, not this ticket (see "Out of scope").
- **Create progress over presence exists, but it cannot tell slow from stalled.** R2.10 publishes a `PROGRESS` entry for a create after 250 ms (ADR 0037 decision 13; `reef-daemon/src/presence/publish.rs:201-265`). Its fixed fields are agent = the create's owner, repo, and the new workspace id. Its progress is `{phase, done, total: 8, unit: "steps"}`, set at each step (`create.rs:147-244`, `CREATE_STEPS` at `:83`). The base step is one `phase("base", 1)` (`:172`) and nothing moves until `phase("materialize", 2)` (`:177`). For the 58 s checkout the entry does not change at all. Its TTL after the last update is 600 s (`publish.rs:22-23`). `reef-daemon/tests/presence.rs:962` (`create_progress`) tests the phases.
- **No test infrastructure makes a create slow.** `reef-daemon/src/testing.rs` has no delay or fault injection, and `crates/reef-testing/src` has none. The only knobs are the synthetic tree's size (`reef_testing::synth::Preset` Small 1k, Medium 10k, Large 100k, Xl 1M files; `synth/mod.rs:90-110`) and `config.presence.progress_after`. No test passes `--timeout` to a create today. `tests/cli.rs:1939` lists it only as a global option.
- **Precedent for a note while waiting.** `ws run` prints `reef: releasing workspace <id>` when the release has run `RELEASE_NOTE_AFTER` (2 s; `ws_run.rs:47-48`, `:495-507`), with `tokio::select!` over the call and a sleep. `Out::note` (`out.rs:161-165`) prints to stderr unless `-q`, in every output form.

## Decisions

### Size

S, with no `dev-phases.md` row. The work is one shared constant and one deadline function, one error variant with its advice, one create helper used by two commands, a deadline change in the MCP tool, a constant swap in the provider, one help string, five unit tests, two integration tests, and two documentation edits.

### The deadline

- **`CREATE_TIMEOUT = 5 min`**, in `cli/mod.rs` beside `REGISTER_TIMEOUT`, `pub(crate)`. Its doc comment: "A workspace create's deadline when `--timeout` is not given: the first create in a repository checks the whole tree out into the base cache, which takes a minute or more on a large one (#188)." 5 minutes is the provider's 300 s, and more than five times the 58 s the issue measured. One constant now covers every create, so the provider's `CREATE_DEADLINE` (`provider.rs:46-48`) is deleted and the provider uses `CREATE_TIMEOUT`.
- **`ws create`, `ws run`, MCP `create_workspace`: `--timeout` replaces the default** (`timeout_flag.unwrap_or(CREATE_TIMEOUT)`), as for register, `ws gc`, `store build` and `backup`. Add `pub(crate) fn create_deadline(timeout_flag: Option<Duration>) -> Duration` in `cli/mod.rs`. A free function can be unit-tested without a `Ctx`. The flag's help already lists per-command defaults that way. A test can also provoke the error with `--timeout 1ms`.
- **The provider keeps "the longer of the two"** (`ctx.timeout.max(CREATE_TIMEOUT)`, at `:468` and `:800`), now on the shared constant. Its behaviour does not change: harnesses call it, and R2.26's contract is out of scope.
- **The help string** (`mod.rs:179-180`) becomes: `How long to wait for the daemon's answer [default: 30s; 5m for a workspace create; 10m for registering a repository; 1h for `ws gc`; for `store build`, how long its command may run, 30m].` It is a hidden flag, so it is only in `--help-all`, which has no byte budget. Register's 10 min was missing from it, and is added now that the line lists per-command defaults.
- **`ws run`'s other calls do not change.** Heartbeats, `prepare` and `finish` keep their deadlines.

### The deadline error

A new variant, so that no other `Timeout` changes:

```rust
/// A workspace create did not answer within its deadline (#188).
#[error(
    "the daemon did not create the workspace within {} (--timeout); it goes on with the create",
    humantime::format_duration(*after)
)]
CreateTimeout {
    /// The deadline.
    after: Duration,
},
```

- **Kind:** `Unavailable`, exit 6, as `Timeout` (`error.rs:265`). There is no reason string, as for `Timeout`.
- **Hint**, one sentence (ADR 0025 rule 2): `The first create in a repository checks the whole tree out into its base cache, which can take minutes on a large one and goes on after the CLI quits: run the command again, which waits for that build instead of starting another, or pass a longer --timeout, such as --timeout 15m.`
- **No `next`.** The fix is to run the agent's own command again, maybe with a flag, and the CLI does not have that command as an argv the agent wrote. ADR 0025 rule 2 says "when the fix needs something only the agent knows, there is a `hint` and no `next`". `ws run`'s command is exactly that.
- **The message says "it goes on with the create"**, not "may still finish". From the trace above, the base build goes on, and the workspace itself is not made in the common case. The message does not say a workspace will appear, so the generic hint's "check (reef ws list) before repeating a create" is not repeated. A retry is safe because the build is shared.
- **`Timeout`'s hint and message do not change.** A deadline in any other call still gets them, and so does the implicit register inside a create's `call_with_repo_within`.
- **`stream.rs:89`** does not list `CreateTimeout`. A create is not a stream.

**How the variant is raised.** `call_with_repo_within` calls `call_within` for the create and also, through `register_implicitly`, for the register. Mapping every `Timeout` after the fact would mislabel a register timeout. Add `Ctx::call_within_as(deadline, timeout: fn(Duration) -> CliError, call)`, which is `call_within`'s body with the error built by `timeout`. `call_within` becomes `call_within_as(deadline, |after| CliError::Timeout { after }, call)`. Add `Ctx::create_with_repo(repo, request)` in `mod.rs`, or in `ws.rs` beside `lease_length`. It is `call_with_repo_within`'s body with `call_within_as(create_deadline(self.timeout_flag), |after| CliError::CreateTimeout { after }, …)` for the two create attempts. `register_implicitly` is untouched. Factor the shared body into one private generic taking the `timeout` function, rather than copying it. Keep the public `call_with_repo_within` signature for its other callers.

### The note

`ws create` and `ws run` print one stderr line when the create has not answered within `CREATE_NOTE_AFTER` = 5 s:

```
reef: creating the workspace; the first create in a repository checks the whole tree out, which can take minutes on a large one
```

- The pattern is `ws run`'s release note (`ws_run.rs:495-507`): `tokio::pin!` the call, then `select!` it against `sleep(CREATE_NOTE_AFTER)`. On the sleep, print the note and await the call. Put it in one helper, `pub(crate) async fn noted<T>(call: impl Future<Output = T>, after: Duration, note: impl FnOnce()) -> T`, beside the create helper. Both commands use it, and it is unit-testable without `Out`.
- `-q` silences it (`Out::note`). It goes to stderr in every form, as the release note does, so `ws create`'s stdout is still one JSON value.
- Not in the provider, whose stderr is a harness's log, or in MCP, where there is no stderr to read.
- 5 s, not 2 s like the release note: an ordinary create is 1-2 s (ADR 0023, measured), and up to 10 s under concurrent first scans. That would print the note in some normal creates, which is harmless. No test asserts an empty stderr from a create (`tests/cli.rs:433`, `:453` assert a release's).

This is the cheap half of the issue's second fix: the CLI says what it is waiting for. The other half, giving up only when progress stalls, is the follow-up below.

### Progress over presence: a follow-up

The CLI can follow a create's progress: it can subscribe to `SubscribePresence` with `repos: [repo]`, `agents: [owner]`, `kinds: [PROGRESS]` before calling create. It is not worth doing in this ticket:

- **It cannot detect a stall.** The base step is one phase with one count (`create.rs:172`-`:177`). During the 58 s checkout the entry does not change, so "progress stalled" and "checking out 277k files" look the same. Detecting a stall needs a file count during the checkout on the daemon side (`Repo::materialize_tree`, through jj-lib's checkout). That is daemon work of unknown size, maybe an upstream hook.
- **The CLI does not know the entry to follow.** The workspace id arrives with the response. `ws create`'s default owner `user@host` is not unique, so two concurrent creates' entries cannot be told apart. `ws run`'s `user@host:<pid>` is unique.
- **It is a second stream and a reconnect policy** in two commands, against a 0.4.0 tag. That makes it M, not S.

The PR files one follow-up issue: "Follow create progress over presence: the CLI prints the phase and waits while it moves; the base checkout reports a file count". It links #188 and this section.

### MCP

`create_workspace` calls `ctx.call_within_as(create_deadline(ctx.timeout_flag), |after| CliError::CreateTimeout { after }, …)` instead of `ctx.call` (`tools.rs:1017-1029`). Its error object carries the same hint. "Pass a longer --timeout" there means `reef mcp --timeout`, which the harness's MCP configuration sets. "Run the command again" reads as calling the tool again, which is right. MCP clients have their own tool-call timeouts (the `wait_for_operations` description warns past 60 s, `tools.rs:404`). A client that gives up first cancels the call the same way, and the base build still goes on. The tool's description and `tools/list` do not change.

### No API change

No proto change, `API_REVISION` stays 14, and the database schema stays 6. The error object's shape (`kind`, `message`, `hint`) is unchanged, so `reef schema cli` is too. No ADR changes. ADR 0025 rule 2 already lists "`--timeout` reached" among the CLI's own advised errors, and ADR 0024 decision 8 covers register's 10 minutes only.

## Deliverables

- **`crates/reef-vcs/src/cli/mod.rs`**: `CREATE_TIMEOUT`, `create_deadline`, `call_within_as`, the create helper (or in `ws.rs`), the `--timeout` help string.
- **`crates/reef-vcs/src/cli/error.rs`**: `CreateTimeout`, its `advice()` arm and `kind()` arm.
- **`crates/reef-vcs/src/cli/ws.rs`**: `create` uses the helper and `noted`. `CREATE_NOTE_AFTER` and `noted` live here, `pub(crate)`.
- **`crates/reef-vcs/src/cli/ws_run.rs`**: `create` uses the helper and `noted`.
- **`crates/reef-vcs/src/cli/provider.rs`**: `CREATE_DEADLINE` removed; `super::CREATE_TIMEOUT` at `:468` and `:800`, with `.max` kept.
- **`crates/reef-vcs/src/mcp/tools.rs`**: `create_workspace`'s deadline.
- **Tests** 1 to 8 below.
- **`CHANGELOG.md`**, Unreleased, **Fixed**: "`reef ws create`, `reef ws run` and the MCP `create_workspace` tool wait up to 5 minutes for a workspace create, not 30 s, so the first create in a large repository, which checks the whole tree out into the base cache, no longer fails partway. `--timeout` still sets the deadline. A create that runs past 5 s says why on stderr, and one that reaches its deadline says that the build goes on and that running the command again waits for it ([#188](https://github.com/reef-vcs/reef/issues/188))."
- **`docs/dev-tickets/phase-2/README.md`**: the row `| #188 | [issue-188-create-waits-for-first-copy.md](issue-188-create-waits-for-first-copy.md) | S | R2.05, R2.13, R2.26 | none listed |`.
- **Unchanged**: the proto, `API_REVISION`, ADRs, `docs/agent-guide.md`, `docs/workspace-provider.md` (it names no deadline), the SDKs, `reef ws create --help` and `reef ws run --help`, MCP tool descriptions.

## Tests

### `crates/reef-vcs/src/cli/error.rs`, in `mod tests`

Run with `cargo nextest run -p reef-vcs --bins error` (the `reef` bin, not `--lib`).

1. **`create_timeout_explains_the_base_cache`**: `CliError::CreateTimeout { after: Duration::from_mins(5) }` has `kind()` `Unavailable` and `exit_code()` 6. Its `to_string()` contains `within 5m` and `--timeout`. `advice()` is `Some`, its hint contains `base cache` and `--timeout`, and its `next` is `None`.
2. **`timeout_advice_is_unchanged`**: `CliError::Timeout { after: 30 s }`'s message and hint are byte for byte today's (copy them from "What was verified"). This guards every other call.

### `crates/reef-vcs/src/cli/mod.rs`, in `mod tests`

3. **`create_deadline_defaults_to_five_minutes`**: `create_deadline(None)` is 300 s and equals `CREATE_TIMEOUT`. `create_deadline(Some(10 s))` is 10 s, so the flag shortens it. `create_deadline(Some(20 min))` is 20 min. Also `assert!(CREATE_TIMEOUT > DEFAULT_TIMEOUT)`.

### `crates/reef-vcs/src/cli/ws.rs`, in `mod tests` (add the module if there is none)

4. **`noted_notes_a_slow_call_once`**: `noted(async { sleep(200 ms).await; 7 }, 1 ms, || count += 1)` returns 7 with `count == 1`.
5. **`noted_is_silent_for_a_quick_call`**: `noted(async { 7 }, 10 s, …)` returns 7 with `count == 0`. Use `#[tokio::test]` with real time. Both cases are ordered by margins of 200x and more, so they do not flake under load. `reef-vcs` has no tokio `test-util`, so do not add `start_paused`.

### `crates/reef-vcs/tests/cli.rs`

6. **`create_timeout_explains_the_base_cache`** (`// #188 test 6`), beside `every_next_is_a_real_command`:
   - **Setup:** `with_repo()` (Small preset).
   - **`ws create`.** `run(reef(&daemon), &["--timeout", "1ms", "ws", "create", "--repo", &repo.id])`. `failed(&output, 6, "unavailable")` passes. The error's `message` contains `create the workspace within 1ms`, its `hint` contains `base cache` and `--timeout`, and it has no `next` key. Stderr's text form has a `hint: ` line with `base cache`.
   - **`ws run`.** `reef --timeout 1ms -o json ws run --repo <id> -- sh -c 'echo ran'` exits 6 with the same error object on stdout, and `ran` is not printed (`ws run` returns before `spawn`, `ws_run.rs:179`). `--repo` is `CreateTarget`'s (`ws.rs:131-135`), as `tests/ws_run.rs:77-80` uses it.
   - **A retry works and nothing leaked.** `create(&daemon, &repo)` (no `--timeout`) succeeds. Then `-o json ws list --all` lists exactly one workspace, that one. A 1 ms deadline fires before the request is served, so no create got as far as a row. Do not assert `base_was_cached` either way: it depends on how far the dropped create got.
   - The integration test does not check the 5-minute default end to end. A create slow enough to pass 30 s needs a repository far larger than CI can afford, and the daemon has no delay injection. Test 3 checks the selection, and "Verification" checks it by hand.

### `crates/reef-daemon/tests/workspaces.rs`

7. **`abandoned_create_leaves_its_base`** (`// #188 test 7`), a guard for the behaviour this ticket's hint promises:
   - **Setup:** a `TestDaemon::with_synthetic_repo(Preset::Medium)` (10k files). Its first create's base checkout takes well over 100 ms.
   - Subscribe to presence with `kinds: [PROGRESS]` and set `progress_after = Duration::ZERO`, as `create_progress` does (`presence.rs:962-968`). Run `tokio::time::timeout(Duration::from_millis(100), client.create_workspace(request(…)))`. It is `Err(Elapsed)`.
   - Read the stream until a `PROGRESS` event with `ended: true` arrives, within 10 s. The dropped handler ended its entry.
   - A second create with no client deadline succeeds. `ListWorkspaces` with every state lists exactly one workspace, the second.
   - Do not assert on `ws/` directories or on `base_was_cached`: where the 100 ms drop lands depends on the machine (step 2 or 3 nearly always, and the step 4-6 leak is out of scope).

### Existing tests

8. Unchanged and passing: `create_progress` (`reef-daemon/tests/presence.rs:962`), `every_next_is_a_real_command` (`tests/cli.rs:2089`; no new `next`), `help_all_is_the_full_help`, `help_text_is_short` (`mod.rs:1032`; the flag's help is not an `about`), every test in `tests/provider.rs` (the provider's deadline does not change), and `tests/mcp.rs` (`tools/list` is byte-identical).

## Implementation notes

- **Order:** `mod.rs` and `error.rs` with tests 1-3; the helper and `noted` with tests 4-5; the two commands, the provider and MCP; test 6; test 7; CHANGELOG and README.
- **The closure.** `call_with_repo_within` takes `F: Fn(String) -> Fut` and calls it twice. The create helper keeps that shape: `|repo| ctx.client.create_workspace(CreateWorkspaceRequest { repo, ..request.clone() })`, as both commands do now. Wrap the whole helper call in `noted`, so the note counts from the first attempt.
- **Timing tests flake under load** on `main`. Test 6 uses 1 ms and a following create with the full default. Test 7's only timed condition is a 100 ms deadline against a 10k-file checkout, and it reads presence with a 10 s bound.
- **No `cfg(target_os = "linux")` code** is touched. On Linux with the copy backend, step 4 is a full copy as well, so a first create is slower than on APFS. The 5 minutes covers both for the issue's repository size.

## Acceptance criteria

- [ ] `reef ws create` and `reef ws run` give a create 5 minutes when `--timeout` is not given, and `--timeout` when it is (test 3). The provider still takes the longer of `--timeout` and 5 minutes. MCP `create_workspace` gives 5 minutes, or `reef mcp --timeout`.
- [ ] A create's deadline error says the create goes on, and its hint names the base cache and `--timeout`. It is exit 6, `unavailable`, with no `next` (tests 1, 6). Every other deadline error is unchanged (test 2).
- [ ] A create still running after 5 s prints one `reef: creating the workspace; …` line on stderr, unless `-q` (tests 4, 5).
- [ ] A create whose client gave up leaves no workspace, and a retry succeeds (tests 6, 7).
- [ ] No API, schema or SDK change. `api_digest_matches_revision` passes at revision 14. CHANGELOG and the tickets README are updated, and the follow-up issue is filed. `just ci` passes.

## Verification

```sh
cargo nextest run -p reef-vcs --bins error create_deadline noted
cargo nextest run -p reef-vcs --test cli create_timeout every_next help
cargo nextest run -p reef-daemon --test workspaces abandoned_create
cargo nextest run -p reef-daemon --test presence create_progress
just ci
```

By hand, once, with the PR's build: generate an `Xl` synthetic tree (`reef-synth gen --out <dir> --preset xl`; 1M files) and `git init` and commit it, or use any checkout whose first create took over 30 s. Then run `reef ws run -- true` there with a fresh state directory and no `--timeout`. It should print the note after 5 s and succeed. Give the first create's `timings.base_ms` from `reef ws create -o json` in the PR. Then repeat with `--timeout 5s`: it fails with the new hint, and an immediate retry with no flag waits for the running build and succeeds.

## Out of scope

- **Following create progress over presence**, and a file count for the base checkout: the follow-up above.
- **A create cut off in steps 4-7** leaves a directory, jj workspace and state directory until the next daemon start, or, after the row, a leased workspace nobody holds until its lease runs out (35 min by default). This was found while writing the ticket, by reading the code, not by reproducing it. The fix is a cancellation-safe create: an `Undo` that runs on `Drop`, or the handler running `Manager::create` in a spawned task so it always completes or undoes itself. That is a daemon change with its own trade-offs (a finished workspace nobody holds versus none). The PR files it as its own issue, linking this section. The longer deadline makes the window rarer, not closed.
- **The generic `Timeout` hint**, which still tells every other command to check `ws list` "before repeating a create". It is still true for a create through another path, and is left as is.
- **Harness shell timeouts.** Some harnesses kill a shell command after about 2 minutes, before 5 minutes. A killed `ws run` drops its create the same way, the base build goes on, and the next `ws run` waits for it. Nothing in Reef can lengthen a harness's timeout.

## Decided by the user

Decided on 2026-11-03, as the brief recommends:

1. **The default is 5 minutes** (`CREATE_TIMEOUT`), shared with the provider.
2. **The 5 s stderr note is in this ticket.**
3. **`--timeout` on a create replaces the default**, as for register, `ws gc`, `store build` and `backup`. The provider keeps "the longer of the two".
