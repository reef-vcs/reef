# Onboarding benchmark

"Agent-friendly" has to be a number, or it drifts. This benchmark measures what it costs an agent that has never seen Reef to go from nothing to a working `reef ws run`, and whether `reef init` and the installer remove that cost. It is Phase 1's agent exit criterion ([`dev-phases.md`](dev-phases.md)) and the regression test for every later change to `reef guide`, the help text, or an error's hint. The brief is [RX.07](dev-tickets/phase-1/RX.07-agent-onboarding-benchmark.md).

The agent is Claude Code, headless (`claude -p`). Codex is the second harness, added once this one has a baseline.

## Scenarios

Each trial gets a fresh home directory, a fresh Reef state directory (`REEF_STATE_DIR`), and a fresh clone of [`BurntSushi/same-file`](https://github.com/BurntSushi/same-file), pinned at `e7d851bc8e88`. It's small, public, and has a `README.md` spelled exactly that way. `--offline` uses a synthetic repository instead, and rows from it say so.

| | Scenario | Setup | Claude Code | Prompt |
| --- | --- | --- | --- | --- |
| A | In-band only | `reef` on `PATH`, a plain git checkout | `--bare` | [`a.txt`](../crates/reef-testing/onboarding/prompts/a.txt) |
| B | After `reef init` | A's checkout after `reef init --agents claude` | not bare, so `CLAUDE.md` and the skill load | `a.txt` |
| C | From nothing | no `reef` anywhere on `PATH` | `--bare` | [`c.txt`](../crates/reef-testing/onboarding/prompts/c.txt) |
| D | Oriented | the agent is started by `reef ws run` | `--bare` | [`d.txt`](../crates/reef-testing/onboarding/prompts/d.txt) |

A and B ask the agent to append a line to `README.md` in an isolated workspace and to print the workspace id on its last line. C asks the same and adds where to install Reef from. D asks which workspace and change the agent is in.

Every trial runs with `--permission-mode acceptEdits --allowedTools Bash,Read,Edit --permission-prompts none --max-budget-usd 2 --no-session-persistence`, so the agent never waits on a prompt. Anything that would still prompt is denied and counted. The prompts are deliberately plain: a prompt that taught Reef would measure the prompt.

**Scenario C before 0.1.0.** There's no GitHub release for the installer to download yet. Until there is, the harness packs the build under test into a release-shaped directory, as `just installer` does. It points the installer at that directory with `REEF_INSTALL_URL` and `REEF_TARGET`, set in the agent's environment. The agent still has to find and run the README's `curl … | sh` line. Once 0.1.0 is out, the variables go and C's rows start a new baseline.

## What is measured

| Column | Meaning |
| --- | --- |
| Success | Trials that succeeded, out of trials in which the agent ran. A trial succeeds when the agent exits by itself, within budget, and the judge confirms the work (below). Hitting the budget or the 10-minute timeout is a failure. |
| Calls to success | Tool calls up to and including the first `Bash` call that ran `reef ws run` and exited 0. A trial that succeeded another way (`reef ws create`, an edit, `reef ws release`) counts every tool call, and so does D, up to its answer. A failed trial counts as infinitely many, so the median says `failed` when most trials failed. |
| Tool calls | Every tool call, any tool, subagents' included. |
| Failed `reef` | `Bash` calls that ran `reef` and exited non-zero. The exit status is the whole call's, so `reef ws run -- <a command that fails>` counts as well. |
| Turns | Model round trips, from Claude Code's `result` event. |
| Input tokens | Every input token, read from the prompt cache or not. |
| Output tokens | Output tokens, thinking included. |
| Cost | `total_cost_usd` from the `result` event. |
| Wall | From starting the agent to its exit. |
| Escaped to git | Trials in which the agent ran `git worktree`, `git stash`, or `git checkout`: it went around Reef. |

Each number is the median over a row's trials. A `Bash` call "runs `reef`" when one of its simple commands (split at `;`, `&`, `|`, `$(`, and backquotes) has `reef`, by name or path, as its program. Variable assignments, `sudo`, `env`, `timeout N`, and `sh -c` are looked through. `reef ws run` is `reef` followed by `ws run` or `workspace run`, after any global options.

**The judge** uses the daemon's records, not the agent's account of what it did. For A to C, the last line of the agent's answer must name a workspace that `reef ws list --all` reports (other words on the line are fine). `README.md` in that workspace's commit, which is its final snapshot once released, must be the original with something appended. The judge reads it with `git cat-file` from the clone, where jj keeps its commits. For D, the answer must name the workspace `reef ws run` started the agent in and its change, by hex or jj id, at least eight characters.

**Setup failures are not the agent's.** A trial whose clone, `reef init`, or installer mirror failed is reported as a setup failure, kept out of the medians, and noted in the row. So is one where Claude Code exited non-zero without a tool call or any spend: no credential, an API error, or a crash at startup. So is a B trial whose stream's `init` event doesn't list the `reef` skill that `reef init` wrote: Claude Code didn't load the project's files, and the trial would measure A.

## Pins and baselines

Each row is the trials of one scenario that share a date, model, Claude Code version, Reef commit, OS, and repository. The model is pinned per run (`REEF_ONBOARDING_MODEL`, default `claude-sonnet-5-5`). Claude Code is pinned in `onboarding.yml`, and a local run reports whatever version is installed. A row with a new model, a new Claude Code version, or a changed prompt starts a new baseline: compare it with the rows after it, not with the rows before. Say what changed in the Notes column.

Phase 1's targets are medians of three trials. On A, at most eight calls to success and at most two failed `reef` calls (the exit criterion). On B, at most four and zero. These were the brief's first guess. A results pull request may tighten them, but loosening one needs a note in an ADR.

## Running it

```sh
ANTHROPIC_API_KEY=... just onboarding scenario=A trials=3
cat crates/reef-testing/onboarding/results/latest.md
```

`just onboarding` builds `reef`, `reef-daemon`, and the harness (`reef-onboarding`, in `crates/reef-testing`), then runs `reef-onboarding run` against the debug build. With no arguments, it runs every scenario three times. Each trial may spend up to $2, so a full run costs at most $24. It also takes the binary's flags: `--model`, `--offline`, `--keep` (keep each trial's home, state, and clone for reading), `--timeout-secs`, `--budget-usd`, and `--claude`.

The results go to `crates/reef-testing/onboarding/results/`, which git ignores:

- `<date>-<scenario>-<trial>.json`: the pins, the outcome, the metrics, and the verdict;
- `<date>-<scenario>-<trial>.stream.jsonl`: the agent's stream, scrubbed of the API key and any `Authorization` header;
- `<date>-<scenario>-<trial>.stderr.log`, scrubbed the same way;
- `<date>-<scenario>-<trial>.workspaces.json`: what the judge read from the daemon;
- `latest.md`: the run's rows and a line per trial.

`reef-onboarding report --results <dir>...` summarizes saved results again. `--docs docs/onboarding-benchmark.md` adds their rows to this page.

**What a local run isolates, and what it doesn't.** Each trial's environment is cleared. The agent sees its own `HOME`, `REEF_STATE_DIR`, `SHELL`, and a `PATH` with no other `reef` on it, plus the API key, `USER`, `LOGNAME`, locale, temporary directory, proxy, and certificate variables. A `PATH` directory holding a `reef` or `reef-daemon` is replaced for the trial by a directory of symlinks to everything else in it, so the agent keeps the other tools there. Nothing from your `~/.claude`, your shell, or a Claude Code session running the benchmark reaches it. It's not a container, though: the agent has an unrestricted `Bash` tool on your machine. The brief called for a fresh container per trial. A fresh home, state directory, and clone isolate what the numbers depend on, and they work the same on a macOS laptop and a Linux runner.

## CI

[`.github/workflows/onboarding.yml`](../.github/workflows/onboarding.yml) runs on `workflow_dispatch` only, never on a schedule or on pull requests. With Claude Code and the model pinned, a run measures something new only when Reef changed, so dispatch it after a change to what a new agent sees (`reef guide`, error hints and `next`, `reef here`, the installer), before a release, and when a pin moves. It costs money and needs the `ANTHROPIC_API_KEY` repository secret, which pull requests from forks don't get. Give that key a spend limit in the Anthropic console.

The workflow runs one job per scenario on `ubuntu-24.04`, with fuse-overlayfs and the pinned Claude Code. Each job uploads its results directory as an artifact (`onboarding-<scenario>-<run id>`, kept 90 days) and appends its summary to the job summary.

A last job collects the artifacts and adds the rows to this page. It then opens a pull request from `onboarding/<date>-<run id>`, with the commit signed off for the DCO check. Two things to know about that pull request:

- It needs **Settings → Actions → General → Allow GitHub Actions to create and approve pull requests**.
- GitHub doesn't run CI on a pull request opened with the workflow's own token. Close and reopen it to start the checks.

## Results

Newest rows last. Columns are as described in [What is measured](#what-is-measured).

The first baseline (2026-10-24, macOS, 12 trials, $0.27 in all) meets both Phase 1 targets: A's medians are 4 calls to success and 1 failed `reef` call, B's are 2 and 0. Neither of A's failed calls was a `reef` error. In each, the agent `cd`'d into its workspace and released it in the same `Bash` call, and the shell's working directory went away under it. The D trial that failed answered after one shell command (`pwd`, `ls`, `git status`) without running `reef`: nothing on disk in a workspace says it is one, and the agent didn't look at `REEF_WORKSPACE`.

### Scenario A: in-band only

| Date | Model | Claude Code | Reef | OS | Success | Calls to success | Tool calls | Failed `reef` | Turns | Input tokens | Output tokens | Cost | Wall | Escaped to git | Notes |
| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 2026-10-24 | claude-sonnet-5-5 | 2.1.310 | 1956cd007979 | macos | 3/3 | 4 | 4 | 1 | 5 | 17,810 | 467 | $0.02 | 12 s | 0/3 | First baseline. Both failed calls released the workspace the shell was in. |
<!-- onboarding-rows:A -->

### Scenario B: after `reef init`

| Date | Model | Claude Code | Reef | OS | Success | Calls to success | Tool calls | Failed `reef` | Turns | Input tokens | Output tokens | Cost | Wall | Escaped to git | Notes |
| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 2026-10-24 | claude-sonnet-5-5 | 2.1.310 | 1956cd007979 | macos | 3/3 | 2 | 2 | 0 | 4 | 53,985 | 233 | $0.02 | 8 s | 0/3 | First baseline. |
<!-- onboarding-rows:B -->

### Scenario C: from nothing

| Date | Model | Claude Code | Reef | OS | Success | Calls to success | Tool calls | Failed `reef` | Turns | Input tokens | Output tokens | Cost | Wall | Escaped to git | Notes |
| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 2026-10-24 | claude-sonnet-5-5 | 2.1.310 | 1956cd007979 | macos | 3/3 | 5 | 6 | 0 | 7 | 45,816 | 1,179 | $0.04 | 109 s | 0/3 | First baseline. Installer from a local mirror (before 0.1.0). |
<!-- onboarding-rows:C -->

### Scenario D: oriented

| Date | Model | Claude Code | Reef | OS | Success | Calls to success | Tool calls | Failed `reef` | Turns | Input tokens | Output tokens | Cost | Wall | Escaped to git | Notes |
| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 2026-10-24 | claude-sonnet-5-5 | 2.1.310 | 1956cd007979 | macos | 2/3 | 3 | 3 | 0 | 4 | 14,102 | 406 | $0.01 | 10 s | 0/3 | First baseline. Trial 1 answered without running `reef`. |
<!-- onboarding-rows:D -->
