# Issue #122 Live token exit criterion

Phase 2 · Size S for the recommended option (A is S to M, E is M; see "The options") · Fixes: issue #122 (the live half of Phase 2's exit criterion on repository state), or records it as partially met · Depends on: R2.17 (the benchmark, its gate and live harness), #114 (brief JSON and `ws list` counts: the scripted half, met), R1.26 and ADR 0025 (the agent-facing CLI contract, which this amends) · Unblocks: closing Phase 2

This brief lays out options for the owner to choose between. "Recommendation" is the brief writer's, not a decision. "Decisions for the owner" lists what only the owner can settle. The plan describes the recommended option and is short enough to redo if the owner picks another.

## Context

Phase 2's exit criterion: "Token cost to answer 'what is the state of this repo' is measured and at least five times lower than parsing git output." R2.17 evaluates it on `state-repo` ("which other agents have work not on `main` yet, and how many files has each changed?"). It requires two things. The scripted cost must be at most a fifth of git's under bytes, Anthropic's count and `o200k`. The live median answer tokens must be at most a fifth of the `git` arm's, in every harness measured (decided by the user, 2026-10-30). `-o text` never counts.

#114 met the scripted half. `reef ws list` answers in one call: 215 bytes, 118 Anthropic and 87 `o200k` tokens, against git's 2,893, 1,691 and 989 (macOS, PR #124). The user accepted the live half as unmet and tracks it here (2026-10-31).

### The problem, measured

**What the live gate compares** (`crates/reef-testing/src/tokens/live.rs:375-389`, `report.rs:535-600`). For each tool call, *answer tokens* take the call's input and the result text as the harness delivered it, joined as `input + "\n" + result`, all calls joined by `"\n"`. For a shell call the input is the command line; for any other tool it is the input JSON. The joined text is counted once, by the harness vendor's counter: for Claude Code, Anthropic's `count_tokens`, sent as one user message, less a one-character message (`count.rs:76-166`).

- **Not counted**: the prompt; the harness's system prompt and tool definitions; anything injected as instructions; the agent's own output and reasoning; and turn-by-turn rereads (cache reads, billed input).
- **The gate**: git's median divided by the Reef arm's median, both over correct trials, must be at least 5, with at least 2 of 3 trials correct in each cell.
- **The budget is a fifth of the `git` arm's median in the same run**:
  - macOS: about **448** (git 2,242);
  - Linux: about **383** (git 1,914; the three trials were 1,793, 1,914 and 1,940, so 359 to 388). CI publishes on Linux.

Two consequences decide which options can work:

1. **Text the agent runs is counted in full, once.** That includes `reef --help`.
2. **Text injected into the session's context is not counted at all.** It shows only in the "session overhead" column (the first request's input).

**Where the live tokens go today.** The latest live run is Linux run 41163502205 (Reef 505bfab, before #114, PR #121). Its artifacts were downloaded, and each trial's counted text was rebuilt from the saved stream. The rebuilt texts match the recorded `answer.bytes` exactly. Measured:

- **The first call is always help.** All 18 of 18 `reef`-arm trials, across all six questions, start with `reef --help`. In 15 the agent read the whole 3,159 bytes; in 3 it piped through `head -40` and read 2,800. No trial ran bare `reef`, `reef guide` or `reef here`. Session overhead is 1,540 tokens per trial for the harness, and is not counted.
- **The `state-repo` trials** went:
  1. `reef --help`;
  2. `reef presence; reef ws list` in 3 of 3 trials. `presence` without a subcommand prints its 1.7 KB help and an error;
  3. then `log`, `show` or `status` loops.

  Answer tokens were 10,281, 12,512 and 16,047, in 4 to 5 calls.
- **Git's `state-repo` trials** ran `git branch -a -vv; git worktree list; git log --oneline -3`, then per-worktree `status --porcelain`, then `| wc -l`: 3 to 4 calls, 3,095 to 3,461 bytes, and 1,793 to 1,940 tokens. That is about twice git's scripted steelman (2,593 bytes, 1,505 Anthropic tokens on Linux). The live git agent explores too.
- **Cost of that run**: $1.09 for all 54 trials, and $0.18 for the 9 `state-repo` trials (summed from each trial's `cost_usd`). R2.17's estimate of $2 to $8 per run was high.

**What the agent reads, now.** Measured with the installed `reef 0.2.0 (5a72635)` on macOS. Bytes are `wc -c`. `o200k` is `tiktoken-rs` 0.12.1, the benchmark's own crate and version; it gives 725 for the 3,159-byte help in the streams, as #114's brief did.

| Text | bytes | o200k | Command |
| --- | ---: | ---: | --- |
| `reef --help` (50 lines) | 3,547 | 817 | `reef --help` |
| · its command list (29 lines) | 2,010 | 434 | |
| · its global options (13 options) | 1,340 | 334 | |
| · header and footer | 197 | 49 | |
| `reef --help` at 505bfab (in the streams) | 3,159 | 725 | from run 41163502205 |
| bare `reef` = `reef guide` | 1,566 | 434 | `reef`, `reef guide` |
| `reef here`, piped, in a workspace (full compact JSON; no brief view yet, #123) | 1,102 | 398 | `reef here` |
| `reef here -o text` | 393 | 126 | |
| `reef ws` with no subcommand | 2,181 | 525 | `reef ws` |
| `reef presence` with no subcommand | 1,718 | 419 | `reef presence` |
| `reef ws --help` | 2,050 | 491 | |
| `reef ws list --help` | 1,810 | 455 | |
| `reef init`'s `AGENTS.md` section, markers included | 653 | 160 | `AGENTS.md` at the root |
| `reef ws list` on the fixture, with the command line (PR #124) | 215 | 87 | Anthropic: 118 |
| MCP `list_workspaces` definition alone | 979 | 222 | `tools/list` over `reef mcp` |

- **The help grew 12% in two tickets without a test noticing.** It went from 3,159 bytes to 3,547: #114's `-o` text added 290, and R2.24's `schema` line added 98 (#122's comment). Only lines are budgeted (60, `tests/cli.rs:1648`).
- **The global options block (1,340 bytes) is repeated in every subcommand's help**, which is why `ws list --help` is 1.8 KB.

**Anthropic tokens per `o200k` token.** Estimated from the same 54 trials. Whole-trial texts run 1.34 to 1.72 Anthropic tokens per `o200k` token. JSON-heavy texts (the `reef` and `mcp` arms) are at 1.34 to 1.45. Git's text is at 1.50 to 1.72. MCP's tool definitions are at 1.69 (R2.17's fixed-cost row). Fitting the 18 `reef` trials as one ratio for the help and one for the rest gives 1.72 for help and 1.34 for the rest. This brief therefore estimates help-like prose at **1.6 to 1.75** Anthropic tokens per `o200k` token, and brief JSON at the measured 1.36 (118 / 87). The estimates below are `o200k` counts of real or drafted text times that range. None of them is an Anthropic count.

**Today, after #114, unchanged** (estimate):

- The best path is `reef --help` (817 × 1.6 to 1.75 = 1,310 to 1,430) plus `reef ws list` (118): **1,430 to 1,550**. That is 1.2 to 1.3× on Linux and 1.4 to 1.6× on macOS.
- With the `presence` detour that 3 of 3 baseline trials took (419 × 1.6 to 1.75 = 670 to 730 more): 2,100 to 2,280, which is 0.8 to 0.9× on Linux.
- No post-#114 live run exists. PR #124 said `tokens.yml` would be dispatched after merge, and it was not (see "Inconsistencies found").

**The arithmetic every option faces.**

- **What is left for help.** On Linux, 383 tokens minus `ws list`'s 118 and about 8 tokens of command lines leaves about **257** for everything else the agent reads. Help-like text runs 3.7 to 4.3 bytes per `o200k` token (measured on today's help and the drafts below). At 1.6 to 1.75 Anthropic tokens per `o200k` token, 257 tokens is **about 147 to 160 `o200k` tokens, or about 550 to 690 bytes of orientation, with no margin**. Keeping 50 tokens of margin brings that down to about 440 to 560 bytes. On macOS about 322 tokens are left: about 680 to 860 bytes.
- **The same arithmetic applies to anything the agent must read once to learn Reef**: help, a guide, or instructions. `AGENTS.md`'s section is 653 bytes. Counted honestly, any orientation of that size lands at about 5× on Linux, give or take 10%.

## The options

Each option is estimated on `state-repo`, `reef` arm, Claude Code, as Anthropic answer tokens. The method is: the drafted or measured text's `o200k` count times 1.6 to 1.75, plus 4 to 6 tokens of command line, plus `ws list` at 118. The verdict compares against the Linux budget, 383, which is the binding one.

### A. Give each arm what its tool installs

**What changes.**

- The `reef` arm starts with `reef init`'s `AGENTS.md` section as session context. The `git` arm gets nothing, since git installs nothing; the `mcp` arm gets nothing beyond its server.
- The harness-neutral way to do it:
  - `Invocation` (`agent/harness/mod.rs`) gains `instructions: Option<&Path>`, filled from `reef init --print`;
  - each adapter maps it to its harness's own mechanism. Claude Code's is `--append-system-prompt-file`, which `--bare` honors (`claude --help`, 2.1.314). No fixture change is needed.
- The other way, writing the files into the fixture and dropping `--bare`, is a flaw:
  - it gives the two copies different histories (R2.17 requires them identical);
  - it loads Claude Code's whole default configuration. RX.07's scenario B, run that way, reads about 53,600 input tokens against about 17,600 for scenario A.
- The short guide must also name `reef ws list`. Without that, an agent with the section still has to look for the command. The line (`reef ws list   # every agent's workspace, files changed`) adds 72 bytes. The guide is at 1,566 of its 1,600-byte budget (`cli/guide.rs:91`), so something else must go out.
- `scenario.rs`'s strip list stays as it is; the instructions arrive through the adapter.

**Estimated live tokens.** It depends on whether the section counts:

- **A1, uncounted.** This is how the code works today, and how #114's brief described it (decision 1(b)). The agent reads the section and runs `reef ws list`: about **118 to 130**, which is **15 to 16× on Linux and about 18× on macOS**. RX.07's scenario B shows that agents given instructions skip exploration: 2 calls against 4. If the agent runs `reef --help` anyway, the answer is about 1,550, or 1.2×.
- **A2, counted as a per-session cost**, as MCP's fixed cost is in the scripted half. Live: the section with the `ws list` line (725 bytes, 174 `o200k`, so 280 to 305) plus 118 gives about **400 to 425**: **4.5 to 4.8× on Linux** and 5.3 to 5.6× on macOS. Scripted: 215 + 725 = 940 bytes against git's 2,593 is **2.8×**, and 87 + 174 = 261 `o200k` against 842 is 3.1×. **Both halves fail.**

**Reaches 5×?** Only as A1, by leaving the orientation uncounted. A2 fails both halves.

**Size.** S to M: the `Invocation` field, the adapter flag, a guide edit inside its budget (with `AGENTS.md`, `CLAUDE.md` and the skill regenerated), tests 10 and 11 of R2.17 updated, docs, and a new baseline.

**Methodology.**

- It **reverses** R2.17's recorded rule "no arm gets `reef init`'s files" (R2.17 brief, "Agent-agnostic"). It also starts a new baseline, because the fixture's setup changes.
- It changes the question being asked: from "an agent that already knows Reef, from its tools alone" to "an agent in a repository set up with `reef init`". RX.07's scenario B already measures the second for onboarding.

**Agent-agnosticism.** Neutral when it goes through `Invocation`: every adapter receives the same text. A flaw when it goes through Claude Code's file discovery.

**Gaming risk.** High for A1. The 280 to 305 tokens of orientation move from counted text into uncounted context. The gate improves by roughly that amount while agents read about as much as before. The honest framing is a per-session cost with a break-even, as R2.17 reports for MCP (A's section repays itself in about 0.2 questions: about 300 against a saving of about 1,800 per question). That framing is A2, and A2 fails.

**Measurement.** A full dispatch for the new baseline, about $1 to $2 by the measured $1.09.

### B. Cut `reef --help` well below 1 KB

**What changes.**

- `reef --help` lists every command by group, without descriptions.
- Global options leave the help that `-h` and `--help` print (shown under a new `--help-all`) in the root and in every subcommand.
- A missing subcommand prints that short help.

Draft, 530 bytes and 144 `o200k`:

```
Reef: version control built for a world where AI agents write most of the code

Usage: reef [OPTIONS] <COMMAND>

Workspaces: ws (run, create, list, inspect, release, move), here
Changes:    status, log, show, diff, describe, abandon, undo, op, history
Agents:     guide, init, mcp, schema, presence
More:       file, symbols, ui, remote, repo, store, daemon, config, backup,
            restore, completions

`reef <command> --help` for one command; `--help-all` for every option.
New here? `reef guide`. Where am I? `reef here`.
```

**Estimated live tokens.**

- If the agent goes straight from help to `ws list`: 144 × 1.6 to 1.75 = 230 to 252, plus about 5, plus 118 gives **350 to 375**. That is **5.1 to 5.5× on Linux** (4.8 to 5.1× at the cheapest git trial) and 6.0 to 6.4× on macOS.
- Nothing in this help says what `ws list` returns, and "Agents: … presence" invites the detour every baseline trial took. One extra call costs about 150 to 330 tokens even after the trim: `ws --help` without the global options is about 800 bytes, and `presence` without a subcommand about 450. That gives about **500 to 700, or 2.7 to 3.8×**.

**Reaches 5×?** Borderline, and only on the direct path.

**Size.** S (as B+C, below).

**Methodology.** Unchanged: no baseline break, no decision reversed.

**Agent-agnosticism.** Neutral: every agent and every human reads it.

**Gaming risk.** Low.

**Measurement.** One full dispatch, about $1 to $2.

### C. Make the first thing an agent runs point at `reef ws list`

**What changes.** A line naming `reef ws list` for repository state, added to `reef --help`'s current text (C on help) or to bare `reef` and `reef here` (C on the others).

**Estimated live tokens.**

- **C on help alone.** Today's help plus one line is 3,641 bytes and 837 `o200k`, about 1,340 to 1,465 tokens, plus 118: **1,460 to 1,590, or 1.2 to 1.3× on Linux.** It prevents the `presence` detour but does nothing about the help's size.
- **C on bare `reef` and `reef here`.** No effect on this benchmark: no trial ran them (measured, 0 of 18). It would help agents that do, at no cost.

**Reaches 5×?** No.

**Size.** XS.

**Methodology.** Unchanged.

**Agent-agnosticism.** Neutral.

**Gaming risk.** Low on its own.

**Measurement.** One dispatch.

### B+C. A short, task-first `reef --help` that leads with `ws list`

**What changes.** B and C together:

- `reef --help` becomes a hand-written help of at most 512 bytes. It leads with the four commands agents need most, `ws list` first, and names every other command.
- `--help-all` prints today's full help.
- Global options leave every short help.
- A missing subcommand prints the short help.

Draft, 524 bytes and 125 `o200k`:

```
Reef: a workspace per agent, snapshotted as it writes

Usage: reef <command> [--help]

  ws list   every agent's workspace and how many files it changed
  here      the workspace you are in, and what to run next
  ws run    run a command in a new workspace, then release it
  history   who changed a file or symbol, landed or open

Also: status log show diff describe abandon undo op presence file
symbols guide init mcp schema ui remote repo store daemon config
backup restore completions. `--help-all` describes them all.
```

**Estimated live tokens.**

- The help is 125 × 1.6 to 1.75 = 200 to 219. Add `reef --help` and `reef ws list` as command lines (about 8) and `ws list`'s output (118, command line included in that figure): **about 320 to 345**.
- That is **5.5 to 6.0× on Linux** (5.2 to 5.6× at the cheapest git trial) and **6.5 to 7.0× on macOS**.
- The margin on Linux is about 40 to 60 tokens. One detour costs 150 or more, so it fails, but a 3-trial median survives one detour trial out of three.
- The other questions benefit too. Every `reef`-arm trial of every question pays the help, so each saves about 1,100 to 1,200 tokens.

**Reaches 5×?** Plausibly, with a thin margin on Linux.

**Size.** S: no proto, API, daemon or MCP change. The work is the root help text and its plumbing, clap settings on the global options, three budget tests, an ADR amending ADR 0025, and docs.

**Methodology.** Unchanged: R2.17's method, fixture and prompts stay, so the rows continue the post-#114 baseline. No recorded decision is reversed. ADR 0025's decision 3 is amended (help stays within 60 lines and gains byte budgets).

**Agent-agnosticism.** Neutral. It names no harness, detects nothing, and is what every agent and human sees first. Test 2 below adds the prompts' forbidden-word check to the root help.

**Gaming risk.** Low to moderate. The first line answers the benchmark's question almost word for word. It is also Reef's main view of a repository with several agents, and the command an agent would need for any "what is everyone doing" task. Check that the other five questions do not regress. The `history` line serves `who-*`.

**Measurement.** One full dispatch after merge, about $1 to $2. A pending post-#114 dispatch before it gives the "before" (below).

### D. Leave it unmet and close Phase 2 with the criterion documented as partially met

**What changes.** Docs only:

- `dev-phases.md`'s bullet becomes "Partially met: scripted met by #114 (13.5×, 14.3×, 11.4× on macOS); live not met (about 1.3× estimated after #114; measured N× by the post-#114 run), because a live agent reads `reef --help` first: #122".
- `token-benchmark.md`'s verdict paragraph says the same.

**Estimated live tokens.** Unchanged: about 1,430 to 2,280, or 0.8 to 1.3×.

**Reaches 5×?** No, by design.

**Size.** XS.

**Methodology.** Unchanged. It amends the phase's acceptance test (`dev-phases.md` line 5: "a phase's exit criteria are the milestone's acceptance test").

**Agent-agnosticism.** Neutral.

**Gaming risk.** None.

**Measurement.** One dispatch is still owed for the post-#114 numbers, about $0.20 for `state-repo` alone or $1 to $2 for everything.

**D also follows any other option that misses.**

### E. Change what the live gate measures

Three variants, all reversing the user's 2026-10-30 rule: "If the scripted ratio holds and the live one misses only because agents do not find the cheap command, the verdict is 'not met'" (R2.17 brief, "Phase 2's exit criterion").

- **E1. Count orientation calls as session overhead.** Calls to `--help`, `guide` or `here` would come out of answer tokens.
  - Live: about 118, or 16×.
  - Gaming risk: high. Which calls count as orientation is a judgment, and the only call it removes is the one this issue is about.
- **E2. Several questions per session.** Ask `state-repo` after another question, so help is paid once.
  - Live: about 1,400 / 6 + 118 ≈ 350, about 5.5× on Linux. That is no better than B+C.
  - Size: M. The harness would need to attribute calls to questions, multi-answer grading, and a new git baseline.
  - Gaming risk: moderate. It is a fair model of real sessions, but it is not the criterion's single question.
- **E3. Gate live on bytes, or on `o200k`.** No help: the ratios move together. On Linux, git's live text is 3,296 bytes, and today's help plus `ws list` is about 3,780 bytes, 0.9×.

Methodology for all three: a new baseline and a reversed decision. Measurement: one or two full dispatches. **Not recommended.**

### F. The MCP arm (#116)

**What changes.** The `mcp` arm passes through #116's fixed-cost work.

**Estimated tokens.**

- **Live**: `list_workspaces` now returns about 231 bytes (#114's brief). **The live gate adds no fixed cost to the `mcp` arm** (`report.rs:553-560`; see "Inconsistencies found"). So the `mcp` live half may already pass:
  - about 100 to 130 tokens, or 15 to 19×, if the agent stops after `list_workspaces`;
  - about 1,100 to 1,300, or 1.5 to 1.7×, if it also calls `query_changes`, as 3 of 3 baseline trials did (2,763 bytes).
- **Scripted**: `mcp` plus its fixed cost must be at most 518 bytes on Linux. `list_workspaces`'s output takes 231 of them. The definition of `list_workspaces` alone is 979 bytes (measured), and `tools/list` is 21,211.

**Reaches 5×?** Not credible. No useful tool list fits in about 290 bytes. Deferred loading would move the cost into the harness's own tool search, which is harness-specific and still loads the definition when it is used. #116 stays its own issue (break-even, not this gate).

### Combinations

- **A1 + B+C.** A1's numbers. B+C makes the result independent of A, so A adds a reversed decision and a new baseline for no gain beyond B+C's margin.
- **B+C, then A1 if B+C misses.** That would be choosing the method after seeing the result. If the owner wants A1, decide it before the B+C run (decision 3).

## Comparison

Live estimates: Anthropic answer tokens on `state-repo`, `reef` arm, and the resulting git ÷ Reef ratio. Linux budget 383, macOS 448.

| Option | What changes | Est. live tokens | Est. ratio, Linux / macOS | Reaches 5×? | Size | R2.17 method | Agent-agnostic | Gaming risk | Dispatches (measured $1.09 per full run) |
| --- | --- | ---: | ---: | --- | --- | --- | --- | --- | --- |
| Today (after #114) | nothing | 1,430–2,280 | 0.8–1.3× / 1.0–1.6× | no | – | – | – | – | 1 owed (≈$0.20–2) |
| A1 | `AGENTS.md` section in context, uncounted; guide names `ws list` | 118–130 | 15–16× / ≈18× | yes | S–M | reverses "no init files"; new baseline | neutral via `Invocation`, flawed via file discovery | high: orientation leaves the count | 1 full (≈$1–2) |
| A2 | as A1, the section counted per session | 400–425 | 4.5–4.8× / 5.3–5.6×; scripted 2.8× | no | S–M | as A1 | as A1 | low | 1 full |
| B | grouped help ≤ 530 bytes, global options moved to `--help-all` | 350–375 direct; 500–700 with a detour | 5.1–5.5× / 6.0–6.4× direct | borderline | S | unchanged | neutral | low | 1 full |
| C | one line pointing at `ws list` | 1,460–1,590 | 1.2–1.3× / 1.4–1.5× | no | XS | unchanged | neutral | low | 1 full |
| **B+C** | **task-first help ≤ 512 bytes, `ws list` first** | **320–345** | **5.5–6.0× / 6.5–7.0×** | **plausibly; margin about 50 tokens on Linux** | **S** | **unchanged; amends ADR 0025** | **neutral** | **low to moderate** | **1 owed + 1 full (≈$2–4)** |
| D | docs only | as today | as today | no | XS | unchanged; amends the phase's acceptance test | neutral | none | 1 owed |
| E1 | orientation calls not counted | ≈118 | ≈16× | yes | S | reverses the 2026-10-30 rule; new baseline | neutral | high | 1 full |
| E2 | six questions per session | ≈350 | ≈5.5× | borderline | M | reverses; new baseline | neutral | moderate | 1–2 full |
| F | MCP fixed cost (#116) | live ≈100–1,300; scripted ≥ 1,210 bytes | live 1.5–19×; scripted ≤ 2.1× | no | M+ | unchanged | neutral | – | – |

## Recommendation

*The brief writer's recommendation, not a decision.*

**B+C, measured with R2.17's method unchanged, with D as the pre-agreed outcome if it misses.**

- **It helps every agent, in every repository.** An agent's first contact is `reef --help` (18 of 18 trials). Today that costs about 1,400 tokens, three times the whole budget, and a third of it is global options. Each `reef`-arm trial of every question would save about 1,100 to 1,200 tokens, whether or not the gate passes.
- **It reverses nothing.** The question stays "what does an agent that knows only its tools pay?" There is no new baseline, and the 2026-10-30 discoverability rule is exactly what it obeys.
- **It is the only option that can pass with orientation counted.** A2 shows that any honest per-session orientation of about 650 bytes lands at 4.5 to 4.8× on Linux. B+C's help is 524 bytes and goes straight to the answer.
- **The margin is thin and should be said so in advance.** The estimate is 320 to 345 against 383. The thin part is the help's Anthropic count (no `count_tokens` call was made for this brief), and whether agents skip the detour.
- **One full run after merge is the verdict.** If it misses, the criterion is recorded as partially met (D), with the numbers. Re-dispatching until a run passes would turn noise into a verdict.
- **A1 is not recommended.** It passes by moving the guide out of the counted text. The same reasoning would also excuse the MCP arm's 7,550-token fixed cost.

## Decisions for the owner

1. **Keep R2.17's rule that no arm gets `reef init`'s files?** Recommended: **keep**. A counted honestly fails both halves (A2). A uncounted (A1) passes by excluding orientation.
2. **Accept a task-first `reef --help` for humans too, with the full listing behind `--help-all`?** Recommended: **yes**. Every command is still named in the short help, and `reef <command> --help` is unchanged apart from the global options.
3. **If B+C's verdict run misses 5× on Linux, what then?** Choose before the run:
   - (a) D, partially met, and close Phase 2;
   - (b) adopt A1 and re-run;
   - (c) keep Phase 2 open.

   Recommended: **(a)**.
4. **Make the live gate add the `mcp` arm's fixed cost, as the page's wording implies?** Recommended: **yes, in this ticket**. It is a few lines in `report.rs` and a test. It changes no verdict today, since `mcp` fails the scripted half by about 40×. It closes a loophole where a cheap `list_workspaces` could pass the live half alone.
5. **Spend about $2 to $4 on two dispatches?** One is the owed post-#114 run, `state-repo` only (about $0.20) or full (about $1 to $2); the other is the post-change full run. Recommended: **yes**, with the post-#114 run full, so every question gets a post-#114 row.
6. **The root help's byte budget: 512 bytes?** Recommended: **512**. It leaves about 40 to 60 tokens of margin on Linux. 600 bytes would leave under 30.

### Owner decisions (2026-11-01)

1. **B+C**, with R2.17's method unchanged. R2.17's "no arm gets `reef init`'s files" stands. The task-first help is what humans see too, with the full help behind `--help-all`.
2. **If the verdict run misses 5× on Linux: D.** Record the criterion as partially met with the numbers and close Phase 2. No re-dispatch, no switch to A1.
3. **The live gate adds the `mcp` arm's fixed cost**, in this ticket.
4. **Both dispatches.** The owed post-#114 run was dispatched before implementation: run 41346946845, all questions, 3 trials.
5. **Fix the stale claims in this ticket**, not as follow-ups: `dev-phases.md` and `token-benchmark.md` ("tracked by #114"), and the guide's "about 400 tokens" (ADR 0025, `guide.rs`, R1.26's row), restated in bytes with the measured counts.
6. The root help's budget is **512 bytes** (the recommendation; not asked separately).

**Measured after the brief** (Anthropic `count_tokens`, `claude-sonnet-5-5`, each text as one user message less a one-character message, as `count.rs` does; free):

| Text | bytes | Anthropic | Brief's estimate |
| --- | ---: | ---: | ---: |
| B+C draft help | 524 | **170** | 200–219 |
| B draft help | 530 | 197 | 230–252 |
| `reef --help` today | 3,547 | 1,196 | 1,310–1,430 |
| `reef presence` (no subcommand) | 1,718 | 642 | 670–730 |
| `AGENTS.md` (whole file) | 653 | 231 | 280–305 (with the `ws list` line) |
| `reef guide` | 1,566 | 648 | 700–760 |

Help-like text runs about 1.36 to 1.47 Anthropic tokens per `o200k` token, not 1.6 to 1.75: the fit over trial texts overstated it. B+C on `state-repo` becomes about 170 + 8 + 118 = **296**, which is **6.5× on Linux** (budget 383, margin about 87) and 7.6× on macOS. One `presence` detour after the trim (about 450 bytes, about 170 tokens) would still miss. Today's best path is about 1,196 + 118 + 8 = 1,322, 1.4× on Linux.

## Plan for B+C

### Scope

- **Root help.** `reef --help`, `reef -h` and `reef help` print a hand-written text of at most 512 bytes, in the shape of the draft above:
  - `ws list` first, then `here`, `ws run` and `history`, each with one line;
  - every other top-level command by name;
  - one line naming `--help-all`.

  The text lives in one file embedded with `include_str!`, as `docs/agent-guide.md` is. Suggested path: `crates/reef-vcs/src/cli/help.txt`.
- **`--help-all`** prints today's full clap help, at the root and on every subcommand, global options included. clap 4.6.7 has `ArgAction::HelpShort` and `HelpLong` and `Arg::hide_short_help`. The mechanism is the implementer's: those, or the raw argv pre-parse that `output_flag` already does (`output_mode` and `output_flag`, `cli/mod.rs:756-790`).
- **Global options** (`--state-dir`, `--socket`, `--config`, `--no-autostart`, `--no-register`, `--timeout`, `-q`, `-v`, `--agent`, `--remote`) are hidden from every short help and kept in `--help-all`. `-o` stays visible everywhere, because agents need `-o json`.
- **A missing subcommand** (`reef ws`, `reef presence`, `reef op`) prints the short help of that noun. Today these print 1.7 to 2.2 KB.
- **Unchanged**:
  - `reef guide` and its short form, so `AGENTS.md` does not change;
  - `reef here`'s output (#123 owns brief views);
  - every command's behavior, its `about` line and its examples;
  - completions, which still list every command and option;
  - `reef schema cli`, which still describes every input.

### Files

- `crates/reef-vcs/src/cli/mod.rs`: the root's help handling, `--help-all`, and `hide_short_help` on the global options.
- `crates/reef-vcs/src/cli/help.txt` (new): the root help.
- `crates/reef-testing/src/tokens/report.rs`: the `mcp` fixed cost in the live gate (decision 4), in the harness vendor's counter.

### Tests

1. **`root_help_fits_its_budget`** (`tests/cli.rs`):
   - `reef --help`, `-h` and `help` print the same text;
   - at most 512 bytes and 15 lines;
   - the first command line starts with `ws list`;
   - the text is byte-identical to `help.txt`, so the golden file is the source.
2. **`root_help_names_every_command`**:
   - every visible top-level subcommand in clap's tree appears as a word in the root help, so a new command fails the test until it is listed (the same walk as `leaf_commands`);
   - the text contains none of R2.17's forbidden words (`claude`, `codex`, `cursor`, `opencode`, `gemini`, `anthropic`, `openai`, `gpt`).
3. **`help_all_is_the_full_help`**: `reef --help-all` contains every top-level `about` and every global option. `reef ws list --help-all` contains `--state-dir`.
4. **`short_help_fits_its_budget`**: for every command path that `help_fits_sixty_lines` walks:
   - `--help` stays within 60 lines (the existing test);
   - `--help` is at most N bytes, where N is the largest measured after the change, rounded up to 128 and recorded in the ADR (estimate: about 1,200 bytes; `ws list` drops from 1,810 to about 550);
   - no global option except `-o` appears.
5. **`missing_subcommand_is_short`**: `reef ws`, `reef presence` and `reef op` with no subcommand exit 2 and print at most N bytes.
6. **`completions_list_every_command`**: `reef completions bash` still names every subcommand and every global option.
7. **`live_gate_adds_mcp_fixed_cost`** (`report.rs` unit tests, decision 4): a live `mcp` cell whose answer tokens are under a fifth of git's, but not once the fixed cost is added, does not meet the gate. The `reef` arm is unaffected.
8. **Unchanged and passing**: `help_fits_sixty_lines`, `every_command_completes_with_stdin_closed`, `guide_fits_its_budget`, `just agent-files`, and the `tokens-check` 5× guard.

### Docs

- **`docs/adr/0051-task-first-help.md`** (next number after 0050): the root help, the byte budgets, `--help-all`, and why (these numbers). It amends ADR 0025 decision 3. Add "Amended by: 0051" to 0025, and index it.
- **`CHANGELOG.md`**, under Unreleased, "Changed": `reef --help` is a short task-first list; `--help-all` prints the full help; global options are shown only there.
- **`docs/token-benchmark.md`**:
  - "What is measured": the exact definition of answer tokens from "The problem, measured", including that injected context is not counted;
  - the exit-criterion section: the live `mcp` gate includes the fixed cost (decision 4);
  - the verdict paragraph: left to the results PR.
- **`docs/dev-phases.md`**: the exit-criterion bullet now says "Tracked by #114" and quotes only the first baseline. In this PR it becomes "scripted met by #114 (13.5×, 14.3×, 11.4×, macOS); live tracked by #122". The results PR then writes the verdict.
- **The guide's token claim** (owner decision 5): ADR 0025, `cli/guide.rs:93` and R1.26's row in `dev-phases.md` restate the guide's budget in bytes (1,600) with the measured counts (1,566 bytes, 434 `o200k`, 648 Anthropic), instead of "about 400 tokens".
- **`docs/dev-tickets/phase-2/README.md`**: this brief's row.
- **`README.md:89`** ("`reef --help` lists the rest") stays true.

### Measurement and verdict

1. **Before implementation**, dispatch the run PR #124 promised:

   ```sh
   gh workflow run tokens.yml -f trials=3 -f note="after #114 (brief JSON, ws list counts); owed since #124"
   ```

   Dispatched 2026-11-01 as run 41346946845. It costs about $1 to $2. It gives the measured "before" for this ticket, and checks this brief's estimate of help plus `ws list` at about 1,430 to 1,550 Linux tokens. If it lands well outside that range, revise the estimates here before building.
2. **Locally, before merge** (optional, about $0.10):

   ```sh
   just tokens live question=state-repo arm=reef trials=3
   ```

   Read each trial's calls. The point is to see whether agents go from help straight to `ws list`.
3. **After merge**, one full dispatch:

   ```sh
   gh workflow run tokens.yml -f trials=3 -f note="#122: task-first reef --help"
   ```

   Its results PR records the verdict in `token-benchmark.md` and `dev-phases.md`. If met, write "met (Claude Code only; one harness: a gap, not a proof, #119)". If not met, record what decision 3 chose, with the numbers. **No re-dispatch to find a passing run.**

### Size and order

S, in this order:

1. the root help and its tests (1, 2);
2. `--help-all` and the global options (3, 4, 6);
3. the missing subcommand (5);
4. the live gate fix (7);
5. ADR and docs.

There is no `cfg(target_os = "linux")` code, no proto change and no API revision.

## Out of scope

- **A and E** (the method changes), unless the owner picks them.
- **The short guide and `AGENTS.md`'s section.** Naming `reef ws list` there would help agents in repositories set up with `reef init`, but the guide is at 1,566 of 1,600 bytes. That is a follow-up.
- **Brief views for `here`, `ws inspect`, `log` and the rest** (#123).
- **MCP's fixed cost** (#116), **a second harness** (#119), and **`reef mcp --help`'s harness-named example** (#118). The example stays in `--help-all`.
- **Prompts, questions and the fixture.** A prompt that taught Reef would measure the prompt.

## Follow-ups

- Make the short guide name `reef ws list`, trading 72 bytes inside the 1,600-byte budget, measured on RX.07's scenario B.
- A second harness (#119). Until there is one, any verdict is one harness's.
- If B+C passes with a thin margin, add `reef --help` to the scripted `state-repo` candidates (`reef --help`, `reef ws list`). That reports the discovery path's cost on every run for free. Not gated: against git's scripted steelman (2,593 bytes on Linux) it is only about 3.4× cheaper by bytes, because the live gate's denominator is git's live text, about twice the steelman.

## Inconsistencies found while writing this brief

1. **The live gate does not add MCP's fixed cost.** The page says "for one gated arm (`reef (default)`, or `mcp` with its fixed cost added once)", and both halves follow. `report.rs:546-560` adds the fixed cost to the scripted ratio only. The live `mcp` ratio is answer tokens alone: 0.59× in PR #121 is 1,914 / 3,249, and the session overhead of about 8,000 is not in it. See decision 4.
2. **`dev-phases.md`'s exit-criterion bullet is stale.** It says "Not met … Tracked by #114". #114 has merged, with the scripted half met on macOS (PR #124), and the live half is now #122. `token-benchmark.md`'s verdict still says "The default output's size is tracked by #114".
3. **The post-#114 dispatch never happened.** PR #124 says "`tokens.yml` is dispatched after merge". The only run is 41163502205, at 03:14 on 2026-10-31, an hour before #124 merged at 04:30. Its results PR, #121, is still open. No row exists after #114.
4. **`reef --help` grew 12% (3,159 to 3,547 bytes) with no test failing.** Only lines are budgeted. #114's brief said "`reef --help` must stay within 60 lines" and added 290 bytes; R2.24 added 98.
5. **The guide's token claim.** ADR 0025, `guide.rs:93` ("About 400 tokens at four bytes a token") and R1.26's row ("under 400 tokens") don't match the measurement. The guide is 1,566 bytes, 434 `o200k` tokens (3.6 bytes per token), and an estimated 700 to 760 by Anthropic's count.
6. **The prompt's preamble.** R2.17's brief says every prompt starts "… You are in yours." `tokens/prompts/preamble.txt` and the page say "You are `bot-0`, in yours." The code and page agree; the brief is out of date.
7. **Git's Anthropic count on `state-repo`, macOS**, is 1,673 on the page and in #114's brief, and 1,691 in PR #124. It is the same text, so the counting endpoint drifted. Live budgets come from the same run's git arm, so the gate is unaffected, but the ratios published from different runs are not exactly comparable.
8. **#122's issue text** puts `reef --help` at "roughly 1,000–1,200 Anthropic". The fit over 18 trials gives 1.72 Anthropic tokens per `o200k` token for help text. That makes the 3,159-byte help about 1,160 to 1,270, and today's 3,547 bytes about 1,310 to 1,430 (estimates).

## Departures in the implementation

- **The root help is 509 bytes, 165 Anthropic tokens**, not the draft's 524 and 170: the draft was over its own 512-byte budget. `here` reads "where you are, and what to run next" and the last line "`--help-all` describes each."
- **Every command's `--help` budget is 2,176 bytes**, not about 1,200: `ws run`'s help is 2,056 bytes once the global options leave it (its own options and `long_about` are long). The sum over every command fell from 112,609 to 55,898 bytes.
- **`--help-all` is a visible global flag**, listed in every command's help, so an agent in any command's help sees how to get the rest.
- **`missing_subcommand_is_short`** holds a noun without its verb (stdout and stderr together, as an agent's shell sees them) to the same 2,176 bytes; `reef ws` is 1,316, `reef presence` 853.
- **`quickstart_commands_exist`** checks the quickstart's flags against `--help-all`, since `--version` and the global options no longer appear in `--help`.
