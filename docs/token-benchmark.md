# Token-efficiency benchmark

The design treats tokens as a performance metric: a diff that costs 40,000 tokens to read is slow however fast it was computed. This benchmark measures what it costs an agent that already knows Reef to answer six standard questions about a repository where several agents work: with git porcelain, with the `reef` CLI, and with Reef's MCP server. It is how Phase 2's exit criterion on "what is the state of this repo" is evaluated ([`dev-phases.md`](dev-phases.md)), and the measurement R2.15 and R2.16 left open. The brief is [R2.17](dev-tickets/phase-2/R2.17-token-efficiency-benchmark.md). It sits beside the [onboarding benchmark](onboarding-benchmark.md), which measures reaching a first workspace; this one measures answering once there, and shares its harness.

**Harnesses measured: Claude Code. One harness: a gap, not a proof.** The live part runs through an adapter per harness; a second harness is a follow-up.

## The fixture

One repository is built twice from commits in the Reef checkout the harness runs in, so no network is needed for it. The commits are pinned in [`scenario.toml`](../crates/reef-testing/tokens/scenario.toml); a shallow clone or a rewritten `main` fails setup and names the missing commit.

- **History, identical in both copies.** An "Initial import" of Release 0.2.0's tree, then three commits on `main` by `bot-1`, `bot-2` and `bot-3`, each holding one later commit's diff (a CI change, R2.15's 52 files, a one-file fix). Every agent and harness configuration file (`AGENTS.md`, `CLAUDE.md`, `.claude/`, `.cursor/`, `.codex/`, `.mcp.json`) is removed, so no arm starts with project instructions. Commits are dated at build time.
- **Work in progress.** `bot-4` (4 files), `bot-5` (59 files, 14 new) and `bot-0`, the trial's own agent ([`self.patch`](../crates/reef-testing/tokens/patches/self.patch): one file modified, one added, one deleted).
- **The git copy** holds that work uncommitted in git worktrees on branches `agent/bot-N`, with no `reef` on the agent's `PATH`.
- **The Reef copy** holds it in Reef workspaces, created with `reef --agent bot-N ws create --owner bot-N` and snapshotted. Its landed commits arrive through registration's import, so in Reef's history they have an author but no agent.

Before any trial the harness waits until the history index has caught up and every open session's Rust files have symbols. A fingerprint of both copies (each worktree's status, each workspace's commit, `main`) is taken around every trial; a trial that changed it is kept out of the medians and the fixture is rebuilt. Rows record the fixture's version: a digest of the scenario, the prompts and the self patch.

## The questions

Every prompt starts "Several agents work in this repository at the same time, each in its own checkout, and agents are named like `bot-1`. You are `bot-0`, in yours.", then the arm's tool sentence, "Do not change any files.", the question, and the answer's form. The prompts are in [`tokens/prompts/`](../crates/reef-testing/tokens/prompts/).

| Id | Question | Truth |
| --- | --- | --- |
| `state-own` | Which files have you changed, and what is the base commit's subject? | three paths with statuses; `bot-3`'s subject |
| `state-repo` | Which other agents have work not on `main`, and how many files has each changed? | `bot-4 4; bot-5 59` |
| `changed-small` | Which functions and methods did `bot-4`'s work add, remove or change? | 6 names |
| `changed-large` | The same for `bot-5`'s work under `crates/reef-daemon/src/symbols/` | 10 names |
| `who-file` | Which agents changed `crates/reef-client/src/json.rs`, and is each change landed or open? | `bot-2 landed; bot-4 open; bot-5 open` |
| `who-symbol` | The same for `by_lookup` in `crates/reef-daemon/src/symbols/query.rs` | `bot-2 landed; bot-5 open` |

`state-repo` is the exit criterion's question: in a repository with several agents, "the state of this repo" is who has work in flight and how much. Since #114 it has one-call candidates: `reef ws list` and `list_workspaces` count each workspace's changed files.

**Grading.** The last line starting `ANSWER:` is compared with the truth as an exact set, after normalizing case, Markdown, a leading `./`, a symbol's module or type and `()`, `main` for landed and `in progress` for open, and a subject's trailing `(#NNN)`. For the `changed-*` questions the union of names must match; whether each is in the right category is reported, not required. Medians are over correct trials only, and a cell with fewer than two correct out of three shows `–` and "not answered reliably".

## The arms

| Arm | What the agent has | Tool sentence |
| --- | --- | --- |
| `git` | a shell and a file reader in `bot-0`'s worktree; no `reef` | "Answer with git." |
| `reef` | a shell and a file reader in `bot-0`'s workspace, `reef` on `PATH` | "Answer with the `reef` command." |
| `mcp` | only the Reef MCP server's tools, started in `bot-0`'s workspace | "Answer with the reef tools." |

`reef (default)` counts what a shell gets on a pipe: brief JSON since #114 ([ADR 0045](adr/0045-brief-json-by-default.md)), every field before. Symbol-level diff and the history queries are not separate arms: an agent picks its commands. In the `reef` arm a shell call that runs `git` is counted as an escape, and in the `mcp` arm any shell call is.

## What is measured

- **Answer tokens**: the text of every tool call and of every result the agent read: for each call, its input (the command line, or the input JSON) and its result as the harness delivered it, all joined and counted once. Help the agent runs counts in full. Not the prompt, the harness's system prompt or built-in tool definitions, which every arm pays alike, nor anything put into the session's context, nor the agent's own output, nor the context each turn rereads.
- **Fixed cost**: the MCP server's tool definitions and instructions, which every session that loads the server pays whether it calls a tool or not, reported apart, with the **break-even**: how many questions a session must answer through MCP rather than the CLI's default output to repay it (the fixed cost over the median saving per question; "never" when MCP saves nothing).
- **Billed usage**: input, cache reads, output and cost, as the harness reports them. It grows with turns, because each turn rereads the context.

**Two parts.** The **scripted** part runs, per question and arm, fixed command sequences or MCP calls ("candidates", in `scenario.toml`) and counts each one's calls and output. Each arm's number is its cheapest candidate whose output names everything the truth does: the steelman, git's included. `reef (default)` is the cheapest without `-o text`, which is what an agent's shell gets, since the CLI writes brief JSON when stdout is not a terminal; `reef (text)` is shown beside it. The **live** part runs real agents, three trials per question and arm, and counts what they actually read. Neither alone would do: the scripted part can't see that agents miss the cheap command, and the live part can't separate Reef's output from an agent's habits.

**Three counters.** `bytes` (UTF-8, vendor-neutral); `anthropic`, Anthropic's token counting endpoint for the pinned model (tool definitions counted as the model sees tools); and `o200k`, OpenAI's published `o200k_base` encoding through `tiktoken-rs`, offline. The ratio between two outputs depends on the tokenizer, so no single vendor's decides anything. Live answer tokens are in the harness vendor's counter, with bytes beside.

**The diff sweep** records, for the 20 commits of `main` up to the fixture's base, the bytes of `git show`, `git show --stat`, `reef diff`, `--stat`, `--symbols` and `--symbols -o text`, and the median ratio of `--symbols` to `reef diff`. **Where the bytes go** breaks the state questions' default JSON down by field, so a follow-up knows what to cut.

## The exit criterion

"Token cost to answer 'what is the state of this repo' is measured and at least five times lower than parsing git output" is evaluated on `state-repo`. It is met when, for one gated arm (`reef (default)`, or `mcp` with its fixed cost added once):

1. **scripted**: its cost is at most a fifth of git's under all three counters, and
2. **live**: in every harness measured, its median answer tokens (for `mcp`, plus its fixed cost in the same counter, since #122) are at most a fifth of the `git` arm's, both cells with at least two correct trials out of three.

`reef (text)` is reported but never gated: agents get JSON (brief since #114) unless they ask. The user decided on both halves (2026-10-30). If agents miss the cheap command, the verdict is "not met" and the follow-up is about discoverability.

**Verdict at the first baseline (2026-10-31): not met.** On `state-repo`, git ÷ Reef was 0.09× by bytes, 0.12× by `anthropic` and 0.10× by `o200k` for `reef (default)`, and 0.09×, 0.13× and 0.12× for `mcp` with its fixed cost. Live answer tokens: 0.18× for the CLI and 0.68× for MCP, in Claude Code only (one harness: a gap, not a proof). Reef's text output alone is 1.8× cheaper than git's; its default JSON is about eleven times dearer. [#114](https://github.com/reef-vcs/reef/issues/114) made the default output brief JSON and met the scripted half: `reef ws list` answers in 215 bytes. The live half is [#122](https://github.com/reef-vcs/reef/issues/122): every live `reef` trial began with `reef --help`, which by #122 was 3,547 bytes and 1,196 Anthropic tokens, three times the live budget, so #122 made it a 509-byte task-first help ([ADR 0051](adr/0051-task-first-help.md)). Every later run's verdict is in its results pull request.

**Decisions this baseline settled for earlier tickets.**

- **The tool list (ADR 0035):** the break-even is 15.2 questions by `anthropic` (4.9 by bytes, 7.5 by `o200k`), more than the six asked: [#116](https://github.com/reef-vcs/reef/issues/116).
- **Call sites by default (R2.15):** at the default of 10 they are 29% of `--symbols`' bytes on `changed-large` and 10% on `changed-small`, under a third: default kept.
- **`--symbols` as agents' default (R2.15):** no. Its JSON is larger than plain `reef diff` on `changed-large` (36.7 KB against 26.8 KB) and the same size on ordinary commits (the sweep's median ratio is 1.02); only `--symbols -o text` is small. Part of #114.
- **Agent matching (R2.16):** no live call passed a pattern to `--by` or `agents`; matching stays exact.
- **Found on the way:** `reef history -o text` gives no author for changes without an agent ([#115](https://github.com/reef-vcs/reef/issues/115)); MCP paths refuse a trailing `/` ([#117](https://github.com/reef-vcs/reef/issues/117)); `reef mcp --help` names one harness ([#118](https://github.com/reef-vcs/reef/issues/118)); a second harness is [#119](https://github.com/reef-vcs/reef/issues/119).

## Pins and baselines

Each row records the Reef commit and the fixture's version; live rows add the model (`REEF_TOKENS_MODEL`, default `claude-sonnet-5-5`), the harness and its version (pinned in `tokens.yml` with `onboarding.yml`'s), and the OS. A new pin, prompt, question or fixture starts a new baseline: compare a row with the rows after it. Say what changed in the Notes column.

## Running it

```sh
just tokens scripted                                   # free; ANTHROPIC_API_KEY adds the anthropic counter
ANTHROPIC_API_KEY=... just tokens live question=state-repo trials=1
cat crates/reef-testing/tokens/results/latest.md
just tokens-check                                      # fixture, truth and scripted part in bytes
```

`just tokens` builds `reef`, `reef-daemon` and `reef-tokens` (with the `tokenizers` feature, which brings `tiktoken-rs`; ordinary builds and tests never compile it, and `just lint` only type-checks it) and runs a subcommand against the debug build. The fixture is built from the checkout's git objects, so run it from a full clone (`--source` names another). `live` takes `question=`, `arm=` and `trials=`, and the binary's flags: `--model`, `--budget-usd` (default $1 a trial), `--timeout-secs`, `--keep`. A full live run is 54 trials and is expected to cost a few dollars; it can't exceed $54. `reef-tokens fixture` builds a fixture (under `--work-root`, default the temporary directory) and prints its paths, for reading.

Results go to `crates/reef-testing/tokens/results/`, which git ignores: `<date>-scripted.json` with every candidate's counts, `scripted/` with every candidate's saved output, `<date>-live-<question>-<arm>-<harness>-<trial>.json` with its stream and stderr (scrubbed), and `latest.md`. `reef-tokens report --results <dir>... --docs docs/token-benchmark.md` adds their rows to this page.

A local live run isolates as the onboarding benchmark's does, with the same caveat: the agent has an unrestricted shell on your machine.

## CI

[`.github/workflows/tokens.yml`](../.github/workflows/tokens.yml) runs on `workflow_dispatch` only: dispatch it after a change to what agents read (JSON shapes, `reef guide`, MCP texts, `reef diff` or `reef history` output), before a release, and when a pin moves. It shares the `agent-benchmarks` concurrency group with `onboarding.yml`, so the two never compete for one key's rate limit. A `scripted` job and one `live` job per arm run on `ubuntu-24.04`; a last job adds the rows to this page and opens a signed-off pull request from `tokens/<date>-<run id>`. As with the onboarding benchmark's, that pull request needs "Allow GitHub Actions to create and approve pull requests", and CI does not run on it until it is closed and reopened.

## Results

Newest rows last. Rows are added by `reef-tokens report --docs`.

### Scripted

Each arm's cheapest sufficient candidate, counted; "git ÷ best gated" is git over the cheaper of `reef (default)` and `mcp + fixed`.

| Date | Reef | Fixture | Question | Counter | git | reef (default) | reef (text) | mcp | mcp + fixed | git ÷ best gated | Notes |
| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 2026-10-31 | ? | 421f33d6ff31 | `state-own` | bytes | 185 | 15,915 | 451 | 11,542 | 31,480 | 0.01× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `state-own` | anthropic | 88 | 7,459 | 242 | 6,306 | 13,856 | 0.01× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `state-own` | o200k | 54 | 5,653 | 172 | 4,511 | 8,973 | 0.01× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `state-repo` | bytes | 2,893 | 31,915 | 1,590 | 11,436 | 31,374 | 0.09× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `state-repo` | anthropic | 1,673 | 13,550 | 899 | 5,666 | 13,216 | 0.13× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `state-repo` | o200k | 989 | 9,895 | 659 | 3,858 | 8,320 | 0.12× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `changed-small` | bytes | 22,577 | 13,258 | 1,324 | 9,549 | 29,487 | 1.70× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `changed-small` | anthropic | 9,458 | 5,462 | 727 | 4,842 | 12,392 | 1.73× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `changed-small` | o200k | 5,847 | 4,033 | 516 | 3,240 | 7,702 | 1.45× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `changed-large` | bytes | 50,862 | 25,993 | 1,865 | 20,895 | 40,833 | 1.96× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `changed-large` | anthropic | 20,361 | 9,819 | 982 | 10,402 | 17,952 | 2.07× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `changed-large` | o200k | 12,616 | 7,042 | 696 | 6,639 | 11,101 | 1.79× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `who-file` | bytes | 919 | 5,072 | – | 3,860 | 23,798 | 0.18× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `who-file` | anthropic | 538 | 2,385 | – | 2,012 | 9,562 | 0.23× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `who-file` | o200k | 382 | 1,827 | – | 1,460 | 5,922 | 0.21× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `who-symbol` | bytes | 7,933 | 4,925 | – | 3,672 | 23,610 | 1.61× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `who-symbol` | anthropic | 3,543 | 2,237 | – | 1,881 | 9,431 | 1.58× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | ? | 421f33d6ff31 | `who-symbol` | o200k | 2,280 | 1,669 | – | 1,313 | 5,775 | 1.37× | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `state-own` | bytes | 185 | 858 | 399 | 859 | 20,817 | 0.22× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `state-own` | anthropic | 88 | 429 | 218 | 430 | 7,985 | 0.21× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `state-own` | o200k | 54 | 247 | 150 | 247 | 4,714 | 0.22× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `state-repo` | bytes | 2,593 | 215 | 584 | 231 | 20,189 | 12.06× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `state-repo` | anthropic | 1,501 | 115 | 340 | 128 | 7,683 | 13.05× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `state-repo` | o200k | 850 | 80 | 244 | 81 | 4,548 | 10.62× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `changed-small` | bytes | 22,321 | 4,646 | 1,224 | 5,442 | 25,400 | 4.80× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `changed-small` | anthropic | 9,314 | 2,237 | 671 | 2,663 | 10,218 | 4.16× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `changed-small` | o200k | 5,730 | 1,395 | 458 | 1,645 | 6,112 | 4.11× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `changed-large` | bytes | 50,606 | 12,218 | 1,765 | 17,276 | 37,234 | 4.14× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `changed-large` | anthropic | 20,217 | 5,865 | 926 | 8,430 | 15,985 | 3.45× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `changed-large` | o200k | 12,499 | 3,608 | 639 | 5,182 | 9,649 | 3.46× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `who-file` | bytes | 619 | 3,801 | – | 3,838 | 23,796 | 0.16× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `who-file` | anthropic | 366 | 2,005 | – | 2,019 | 9,574 | 0.18× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `who-file` | o200k | 243 | 1,456 | – | 1,466 | 5,933 | 0.17× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `who-symbol` | bytes | 7,633 | 3,631 | – | 3,661 | 23,619 | 2.10× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `who-symbol` | anthropic | 3,371 | 1,872 | – | 1,883 | 9,438 | 1.80× | #122: task-first reef --help |
| 2026-11-02 | 88c31de71f0f | 421f33d6ff31 | `who-symbol` | o200k | 2,141 | 1,317 | – | 1,324 | 5,791 | 1.63× | #122: task-first reef --help |
<!-- tokens-rows:scripted -->

### Fixed cost

| Date | Reef | Tools | `tools/list` bytes | Instructions bytes | bytes | anthropic | o200k | Break-even (questions) | Notes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| 2026-10-31 | ? | 21 | 21,191 | 844 | 19,938 | 7,550 | 4,462 | bytes 4.9, anthropic 15.2, o200k 7.5 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-11-02 | 88c31de71f0f | 21 | 21,211 | 844 | 19,958 | 7,555 | 4,467 | bytes never, anthropic never, o200k never | #122: task-first reef --help |
<!-- tokens-rows:fixed -->

R2.24 publishes every tool's output schema through `reef schema mcp` and does not declare them in `tools/list` ([ADR 0050](adr/0050-published-schemas.md)), so the fixed cost and break-even above are unchanged; declaring them all would make `tools/list` 141,756 bytes instead of 21,211.

### Live: Claude Code

Answer tokens and bytes are medians over correct trials; the other columns are medians over every trial that counted.

| Date | Model | Harness | Reef | OS | Question | Arm | Correct | Answer tokens | Answer bytes | Session overhead | Billed input | Cache read | Output | Cost | Calls | Turns | Wall | Escapes | Notes |
| --- | --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `state-own` | git | 3/3 | 465 | 849 | 1,569 | 3,654 | 3,130 | 304 | $0.005 | 1 | 2 | 6 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `state-own` | reef | 3/3 | 9,376 | 20,399 | 1,599 | 15,267 | 15,259 | 320 | $0.005 | 2 | 3 | 7 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `state-own` | mcp | 3/3 | 6,389 | 11,531 | 8,040 | 22,505 | 16,072 | 215 | $0.022 | 1 | 2 | 5 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `state-repo` | git | 3/3 | 2,242 | 3,927 | 1,543 | 14,223 | 11,636 | 705 | $0.013 | 4 | 5 | 10 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `state-repo` | reef | 3/3 | 12,164 | 27,013 | 1,573 | 47,705 | 42,342 | 1,023 | $0.039 | 6 | 7 | 16 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `state-repo` | mcp | 3/3 | 3,287 | 5,996 | 8,014 | 19,467 | 16,020 | 229 | $0.013 | 2 | 3 | 5 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `changed-small` | git | 3/3 | 4,265 | 8,424 | 1,575 | 15,735 | 10,832 | 922 | $0.022 | 4 | 5 | 12 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `changed-small` | reef | 3/3 | 5,443 | 12,812 | 1,605 | 23,864 | 18,080 | 775 | $0.021 | 5 | 6 | 10 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `changed-small` | mcp | 3/3 | 3,676 | 7,449 | 8,046 | 28,613 | 28,605 | 519 | $0.008 | 2 | 3 | 8 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `changed-large` | git | 3/3 | 10,819 | 24,479 | 1,596 | 28,323 | 17,374 | 1,124 | $0.040 | 4 | 5 | 14 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `changed-large` | reef | 3/3 | 5,922 | 16,300 | 1,626 | 28,321 | 22,115 | 1,043 | $0.028 | 6 | 7 | 15 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `changed-large` | mcp | 3/3 | 12,221 | 27,951 | 8,067 | 64,981 | 52,933 | 1,278 | $0.048 | 4 | 5 | 16 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `who-file` | git | 3/3 | 2,113 | 3,785 | 1,581 | 7,585 | 5,737 | 665 | $0.012 | 3 | 4 | 8 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `who-file` | reef | 3/3 | 4,465 | 10,963 | 1,611 | 10,507 | 10,499 | 420 | $0.005 | 2 | 3 | 9 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `who-file` | mcp | 3/3 | 2,041 | 3,862 | 8,052 | 18,215 | 18,209 | 298 | $0.005 | 1 | 2 | 5 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `who-symbol` | git | 3/3 | 5,676 | 12,035 | 1,597 | 14,235 | 8,027 | 960 | $0.026 | 4 | 5 | 13 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `who-symbol` | reef | 3/3 | 4,679 | 11,502 | 1,627 | 16,809 | 11,947 | 622 | $0.020 | 3 | 4 | 10 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-10-31 | claude-sonnet-5-5 | claude-code 2.1.314 | ? | macos | `who-symbol` | mcp | 3/3 | 1,933 | 3,726 | 8,068 | 18,156 | 18,150 | 319 | $0.006 | 1 | 2 | 5 s | 0/3 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `state-own` | git | 3/3 | 415 | 798 | 1,536 | 3,538 | 3,064 | 312 | $0.005 | 1 | 2 | 6 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `state-own` | reef | 3/3 | 1,715 | 3,800 | 1,563 | 6,735 | 4,910 | 263 | $0.008 | 2 | 3 | 6 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `state-own` | mcp | 3/3 | 439 | 848 | 8,013 | 16,501 | 16,495 | 215 | $0.004 | 1 | 2 | 5 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `state-repo` | git | 3/3 | 1,992 | 3,436 | 1,510 | 10,491 | 8,682 | 666 | $0.013 | 3 | 4 | 10 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `state-repo` | reef | 3/3 | 286 | 735 | 1,537 | 5,218 | 5,210 | 260 | $0.003 | 2 | 3 | 7 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `state-repo` | mcp | 3/3 | 1,743 | 3,271 | 7,987 | 26,108 | 24,271 | 334 | $0.010 | 3 | 4 | 7 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `changed-small` | git | 3/3 | 4,300 | 8,661 | 1,542 | 15,028 | 10,372 | 1,003 | $0.023 | 4 | 5 | 12 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `changed-small` | reef | 3/3 | 4,701 | 12,966 | 1,569 | 12,735 | 7,873 | 647 | $0.016 | 3 | 4 | 10 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `changed-small` | mcp | 3/3 | 2,649 | 5,417 | 8,019 | 26,970 | 26,962 | 577 | $0.014 | 2 | 3 | 9 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `changed-large` | git | 3/3 | 10,373 | 23,243 | 1,563 | 27,701 | 16,639 | 1,143 | $0.041 | 4 | 5 | 14 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `changed-large` | reef | 3/3 | 4,503 | 12,580 | 1,590 | 20,341 | 15,435 | 1,050 | $0.024 | 6 | 7 | 14 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `changed-large` | mcp | 3/3 | 8,492 | 17,450 | 8,040 | 41,341 | 32,725 | 892 | $0.034 | 3 | 4 | 12 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `who-file` | git | 3/3 | 2,602 | 4,922 | 1,548 | 11,527 | 8,540 | 866 | $0.017 | 3 | 4 | 12 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `who-file` | reef | 3/3 | 2,882 | 6,251 | 1,575 | 7,938 | 7,930 | 208 | $0.004 | 2 | 3 | 7 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `who-file` | mcp | 3/3 | 2,011 | 3,840 | 8,025 | 18,131 | 18,125 | 362 | $0.005 | 1 | 2 | 6 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `who-symbol` | git | 3/3 | 4,706 | 10,120 | 1,564 | 10,216 | 5,048 | 731 | $0.020 | 2 | 3 | 10 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `who-symbol` | reef | 3/3 | 3,130 | 7,395 | 1,591 | 10,817 | 7,643 | 463 | $0.013 | 3 | 4 | 9 s | 0/3 | #122: task-first reef --help |
| 2026-11-02 | claude-sonnet-5-5 | claude-code 2.1.310 | 88c31de71f0f | linux | `who-symbol` | mcp | 3/3 | 1,904 | 3,699 | 8,041 | 18,057 | 18,051 | 353 | $0.006 | 1 | 2 | 6 s | 0/3 | #122: task-first reef --help |
<!-- tokens-rows:live-claude-code -->

### Diff sweep

Median bytes per view.

| Date | Reef | Commits | `git show` | `git show --stat` | `reef diff` | `reef diff --stat` | `reef diff --symbols` | `--symbols -o text` | `--symbols` ÷ `reef diff` | Notes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 2026-10-31 | ? | 20 | 153,658 | 1,600 | 170,894 | 11,092 | 151,320 | 9,029 | 1.02 | first baseline, local run; Reef 79ad454 built in a Reef workspace (no git, so no commit in --version) |
| 2026-11-02 | 88c31de71f0f | 20 | 153,606 | 1,600 | 156,445 | 2,391 | 76,351 | 9,029 | 0.57 | #122: task-first reef --help |
<!-- tokens-rows:sweep -->

### Where the bytes go

The state questions' `reef (default)` output, compact JSON bytes by field.

| Date | Reef | Question | Candidate | Field | Bytes | Share |
| --- | --- | --- | --- | --- | ---: | ---: |
| 2026-10-31 | ? | `state-own` | status | `repository_status.active_workspaces` | 3,156 | 27% |
| 2026-10-31 | ? | `state-own` | status | `repository_status.heads` | 2,859 | 25% |
| 2026-10-31 | ? | `state-own` | status | `repository_status.last_operation` | 1,297 | 11% |
| 2026-10-31 | ? | `state-own` | status | `workspace_status.workspace` | 1,056 | 9% |
| 2026-10-31 | ? | `state-own` | status | `workspace_status.parents` | 751 | 7% |
| 2026-10-31 | ? | `state-own` | status | `repository_status.trunk` | 747 | 6% |
| 2026-10-31 | ? | `state-own` | status | `workspace_status.working_copy` | 729 | 6% |
| 2026-10-31 | ? | `state-own` | status | `workspace_status.changed_files` | 319 | 3% |
| 2026-10-31 | ? | `state-repo` | list, diff summary | `files.[].path` | 2,547 | 11% |
| 2026-10-31 | ? | `state-repo` | list, diff summary | `files.[].new_no_final_newline` | 1,764 | 8% |
| 2026-10-31 | ? | `state-repo` | list, diff summary | `files.[].old_no_final_newline` | 1,764 | 8% |
| 2026-10-31 | ? | `state-repo` | list, diff summary | `files.[].invalid_utf8` | 1,260 | 6% |
| 2026-10-31 | ? | `state-repo` | list, diff summary | `files (structure)` | 1,089 | 5% |
| 2026-10-31 | ? | `state-repo` | list, diff summary | `files.[].lines_removed` | 1,071 | 5% |
| 2026-10-31 | ? | `state-repo` | list, diff summary | `files.[].new_type` | 1,071 | 5% |
| 2026-10-31 | ? | `state-repo` | list, diff summary | `files.[].too_large` | 1,071 | 5% |
| 2026-11-02 | 88c31de71f0f | `state-own` | status | `workspace_status.parents` | 181 | 21% |
| 2026-11-02 | 88c31de71f0f | `state-own` | status | `repository_status.trunk` | 177 | 21% |
| 2026-11-02 | 88c31de71f0f | `state-own` | status | `workspace_status.changed_files` | 167 | 20% |
| 2026-11-02 | 88c31de71f0f | `state-own` | status | `workspace_status.working_copy` | 112 | 13% |
| 2026-11-02 | 88c31de71f0f | `state-own` | status | `workspace_status.workspace` | 53 | 6% |
| 2026-11-02 | 88c31de71f0f | `state-own` | status | `repository_status.active_workspace_count` | 26 | 3% |
| 2026-11-02 | 88c31de71f0f | `state-own` | status | `workspace_status (structure)` | 26 | 3% |
| 2026-11-02 | 88c31de71f0f | `state-own` | status | `repository_status (structure)` | 25 | 3% |
| 2026-11-02 | 88c31de71f0f | `state-repo` | list | `[].id` | 69 | 35% |
| 2026-11-02 | 88c31de71f0f | `state-repo` | list | `[].changed_file_count` | 67 | 34% |
| 2026-11-02 | 88c31de71f0f | `state-repo` | list | `[].owner` | 45 | 23% |
| 2026-11-02 | 88c31de71f0f | `state-repo` | list | `(structure)` | 16 | 8% |
<!-- tokens-rows:breakdown -->
