# 0051. Task-first help

- Status: accepted
- Date: 2026-11-01
- Ticket: issue #122
- Amends: [0025](0025-agent-facing-cli-contract.md)

## Context

Phase 2's exit criterion asks that answering "what is the state of this repo" cost at least five times fewer tokens than parsing git's output. [0045](0045-brief-json-by-default.md) met the scripted half: `reef ws list` answers in 215 bytes against git's 2,893. The live half, where a real agent answers the question, was not met, and the reason was not the answer. Every one of the 18 `reef`-arm trials in R2.17's last live run started with `reef --help`, across all six questions. None ran bare `reef`, `reef guide` or `reef here`.

That run served clap's help at 3,159 bytes (3 of the 18 trials read only its first 40 lines); by #122 it had grown to 3,547 bytes and 1,196 Anthropic tokens. The live budget is a fifth of the `git` arm's median answer tokens: about 383 on Linux. The help alone was three times that before the agent asked anything. Of its bytes:

- 2,010 were the command list, one line of `about` per command;
- 1,340 were the options block, 1,063 of them ten global options (`--state-dir`, `--socket`, `--timeout`, `--agent` and the rest) that an agent almost never passes, and the global ones repeated in every command's help;
- nothing said which command answers a question about the repository's state, so all three `state-repo` trials then ran `reef presence` without a subcommand and read another 1.7 KB of help.

0025 budgeted the help in lines (60), not bytes, so it grew 12% over two tickets without a test failing.

## Decision

We will make `reef --help` a short, hand-written, task-first help, and move the rest behind `--help-all`:

1. **The root's help** (`reef --help`, `-h`, `help`) is `crates/reef-vcs/src/cli/help.txt`, embedded with `include_str!` as clap's help template. At most 512 bytes and 15 lines. It leads with the four commands agents need most, one line each, `ws list` first, then `here`, `ws run` and `history`. It names every other command on an "Also:" line and ends by naming `--help-all`. At #122 it is 509 bytes and 165 Anthropic tokens.
2. **`--help-all`**, on the root and on every command, prints clap's full help with every option. It is a global flag, listed in every command's help.
3. **Global options are hidden from `--help`**, all but `-o`, which agents use. Completions and `reef schema` are built from the full tree and still list them.
4. **Byte budgets.** Tests hold the root's help to 512 bytes and the golden file, check that it names every top-level command and no agent harness, and hold every command's `--help` to 2,176 bytes (the largest at #122, `ws run` at 2,056, rounded up to 128) with no global option but `-o`. The 60-line rule stays.

The rest of 0025 decision 3 stands: `reef` alone prints the guide, every `about` is one line, every `long_about` at most five lines.

The guide's budget is restated in bytes only. 0025 called 1,600 bytes "about 400 tokens at four bytes a token". Measured at #122, the guide is 1,566 bytes, 434 `o200k` tokens and 648 Anthropic tokens: help-like English runs about 3.6 bytes per `o200k` token, and more Anthropic tokens than that.

## Consequences

- An agent's first call costs 165 tokens instead of 1,196. Every `reef`-arm trial of every question in R2.17 pays it, so each saves about 1,000 tokens whether or not the gate passes. The estimate for `state-repo` is help plus `ws list`, about 291 tokens (165 + 118 for `ws list` + about 8 of command lines): 6.6× cheaper than git on Linux, with about 92 tokens of margin. One detour, such as `reef presence` without a subcommand (now 853 bytes, from 1,718), uses up the margin. The verdict is the first full `tokens.yml` run after merge, and a miss is recorded as partially met (the owner's decision, 2026-11-01).
- Summed over every command, `--help` went from 112.6 KB to 55.9 KB.
- Humans see the short help too. Every command is still named in it, and `--help-all` is one flag away.
- The help names a command for the benchmark's question almost word for word ("every agent's workspace and how many files it changed"). That is also the main view of a repository with several agents. The other five questions' rows show whether the order costs them.
- A new top-level command must be added to `help.txt` by hand: `root_help_names_every_command` fails until it is.

## Alternatives considered

- **Give the benchmark's `reef` arm `reef init`'s `AGENTS.md` section as session context.** Rejected by the owner (2026-11-01): it passes only by moving the agent's orientation out of the counted text, and it reverses R2.17's rule that no arm gets `reef init`'s files. Counted honestly, as a per-session cost, it lands at about 4.5× on Linux.
- **A one-line pointer at `reef ws list` in today's help.** Rejected: it stops the detour but leaves 1,200 tokens of help, about 1.2×.
- **A grouped list of command names with no descriptions.** About 5.1× on the direct path, but nothing tells the agent what `ws list` returns, so it is as likely to detour as before.
- **Changing what the live gate counts.** Rejected: it reverses the owner's 2026-10-30 rule that the live half measures what agents actually do.
