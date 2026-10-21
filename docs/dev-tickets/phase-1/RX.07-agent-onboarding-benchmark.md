# RX.07 Agent onboarding benchmark

Cross-cutting · Size M · First needed: Phase 1 release · Depends on: R1.26 (the guide and hints it measures), R1.27 (scenario B), R1.28 (scenario C), R1.25, R1.21 · Unblocks: R2.17 (shares the harness and reporting), R2.13 and R2.25 (add their scenarios)

The brief lives with the Phase 1 tickets because that is where it first runs; it is numbered as a cross-cutting ticket in `docs/dev-phases.md`.

## Context

"Agent-friendly" has to be a number or it will drift. R2.17 measures the tokens an agent spends answering questions about repository state once it knows Reef. This ticket measures the step before that: how much it costs an agent that has never seen Reef to go from nothing to a running `reef ws run`, and whether `reef init` and the installer remove that cost. It is the acceptance test for R1.25 to R1.28 and the regression test for every later change to the guide, the help text, or an error's hint.

### What was verified while writing this ticket

From Claude Code's headless documentation as fetched on 2026-10-21:

- `claude -p "<prompt>"` runs non-interactively; `--bare` skips `CLAUDE.md`, skills, hooks, `.mcp.json`, and auto memory, which is exactly "an agent with no Reef configuration"; without `--bare` a `-p` run loads the project's `CLAUDE.md` and skills, which is "after `reef init`".
- `--output-format stream-json --verbose` emits one JSON object per event including every `tool_use` and `tool_result`; the final `result` object carries `num_turns`, `total_cost_usd`, usage, and `permission_denials`. `--max-budget-usd` caps spend. `--allowedTools "Bash,Read,Edit"` or `--permission-mode` avoids prompts; `--permission-prompts none` denies whatever would still prompt rather than waiting. `ANTHROPIC_API_KEY` in the environment is the credential in bare mode.
- Claude Code exits 0 on success and non-zero on failure, and 143 on SIGTERM, so a timeout wrapper can tell the cases apart.
- Codex CLI's `codex exec` has a comparable non-interactive mode; its event format was not verified this session and it is the second harness, added when the first works.

## Decisions

- **Scenarios**, each a fresh container (Linux, the Phase 1 `ubuntu-24.04` image with `fuse-overlayfs`) or a fresh macOS runner state directory, with a small real repository (a clone of a public jj-using project, as R1.24 uses) and a clean `REEF_STATE_DIR`:
  - **A, in-band only**: `reef` on `PATH`, plain git checkout, `claude --bare -p`. Prompt: "Use Reef (the `reef` command) to make this change in an isolated workspace: append a line to README.md. When done, print the Reef workspace id on the last line." No other context.
  - **B, after `reef init`**: the same, after `reef init` ran in the checkout, `claude -p` without `--bare` so `CLAUDE.md` and the skill load.
  - **C, from nothing**: no `reef` on `PATH`, `claude --bare -p`. Prompt adds "Reef is not installed; install it from https://github.com/reef-vcs/reef first." Measures R1.28's installer and README.
  - **D, oriented**: the agent is started by `reef ws run -- claude --bare -p "...tell me which workspace and change you are in"`; measures `reef here` and the environment variables.
- **Metrics per run**, parsed from the stream: tool calls to first `reef ws run` that exits 0 (`calls_to_success`), total tool calls, `reef` invocations that exited non-zero (`failed_reef_calls`), `num_turns`, input and output tokens, `total_cost_usd`, wall time, and `success` (mechanical: `reef ws list --all -o json` shows a workspace whose final snapshot contains the README change, and the last line of the result is that workspace's id). Any `git worktree`, `git stash`, or `git checkout` in a `tool_use` is recorded as `escaped_to_git`.
- **Three trials per scenario**, medians reported, every trial's stream saved as a workflow artifact for reading. The spend cap is `--max-budget-usd 2` per trial, so a full run is under twenty-five dollars; a trial that hits the cap counts as a failure.
- **Phase 1 exit criterion added** (`docs/dev-phases.md`): "On the onboarding benchmark's scenario A, an agent with no Reef configuration reaches a successful `reef ws run` in at most eight tool calls and with at most two failed `reef` invocations, median of three trials." Scenario B's target is four calls and zero failures. The numbers are the brief's first guess; the first real run sets the baseline and the PR that lands it may tighten them, never loosen them without an ADR note.
- **Schedule**: `onboarding.yml` weekly and on `workflow_dispatch`, never on PRs (cost, and an API key is a secret PRs from forks cannot have); the key is a repository secret `ANTHROPIC_API_KEY` with a spend limit set in the console. Also runnable locally with `just onboarding scenario=A trials=1`.
- **Results** go to `docs/onboarding-benchmark.md` in the shape of `docs/benchmarks.md`: what is measured, how to run it, a table per scenario with the dated medians, and a line of what changed between dated rows. The workflow appends a row by opening a PR, as the benchmark workflow does for its baselines (R0.06).
- **The harness is a small Rust binary** in `crates/reef-testing` (`src/bin/onboarding.rs`, `publish = false` already): builds the scenario, runs the agent with a timeout, parses the stream, judges, writes JSON. Not a shell script: parsing `stream-json` and judging snapshot contents through the daemon's client is Rust work this crate already has helpers for.
- **Prompts are fixed in the repository** (`crates/reef-testing/onboarding/prompts/*.txt`) and versioned; a prompt change is a new baseline, noted in the results table.

## Deliverables

### `crates/reef-testing/src/bin/onboarding.rs`, `crates/reef-testing/src/onboarding/`

Scenario setup (temp state dir, repository clone or `synth` fallback when offline, `reef init` for B, `PATH` without `reef` for C), the runner (`claude` with the flags above, `timeout` 10 minutes, `--model` pinned per run from a `REEF_ONBOARDING_MODEL` variable so results compare like with like), the stream parser (serde over the documented event shapes, tolerant of unknown events), the judge, and the report (`results/<date>-<scenario>-<trial>.json` and a Markdown table). Unit tests parse recorded streams checked in as fixtures (secrets scrubbed) and judge recorded end states.

### `.github/workflows/onboarding.yml`, `justfile`

As in Decisions; the workflow uploads every stream and the table, and opens the results PR.

### `docs/onboarding-benchmark.md`, `docs/dev-phases.md`

The page, the exit criterion, and this ticket's row.

## Implementation notes

- Use `--bare` for A, C, and D and `--permission-mode acceptEdits --allowedTools "Bash,Read,Edit"` everywhere, so the agent never waits on a prompt; record `permission_denials` from the result object, since a denial is itself an onboarding failure worth seeing.
- Pin the Claude Code version in the container (`npm install -g @anthropic-ai/claude-code@<version>`) and print it into the results; the harness's own behavior changes shape the numbers.
- Judge from the daemon, not from the agent's words: the last line must match a workspace id that `ws list --all` reports, and the snapshot check reads the final snapshot's tree through `reef_daemon::client` as `tests/ws_run.rs` does.
- Scrub `ANTHROPIC_API_KEY` and any `Authorization` header from saved streams before upload.

## Acceptance criteria

- [ ] `just onboarding scenario=A trials=1` runs locally end to end with a key in the environment and produces a result JSON and table row.
- [ ] Stream parser and judge unit tests pass without a key or network.
- [ ] `onboarding.yml` completes on `workflow_dispatch` for all four scenarios on Linux, uploads artifacts, and opens the results PR.
- [ ] The first baseline row is in `docs/onboarding-benchmark.md` and the Phase 1 exit criterion is met or the gap is listed as a Phase 1 known limitation with the number.
- [ ] `just ci` passes (the harness compiles and its fixture tests run in the ordinary matrix).

## Verification

```sh
ANTHROPIC_API_KEY=... just onboarding scenario=A trials=3
cat crates/reef-testing/onboarding/results/latest.md
```

## Out of scope

- Token cost of repository-state questions once onboarded (R2.17).
- Harnesses beyond Claude Code and Codex; others are added as scenarios when their non-interactive mode is verified.
- Running on PRs; the number is a weekly trend and a release gate, not a merge gate.
- Optimizing the prompt. The prompt is deliberately plain; a prompt that teaches Reef would measure the prompt.

## Risks

- Model and harness updates move the numbers without any change in Reef; pinning the model and Claude Code version per row and reporting the pins with each row keeps the table honest. A row with a new pin is a new baseline, not a regression.
- Cost creep if trials or scenarios grow; the budget cap per trial and the weekly cadence bound it at about a hundred dollars a month.
- A flaky judge (network clone fails, daemon slow to start) would read as agent failure; the harness distinguishes setup failures (exit before the agent starts) from agent failures and reports them separately.
