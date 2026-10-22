# 0025. Agent-facing CLI contract

- Status: accepted
- Date: 2026-10-22
- Ticket: R1.26

## Context

Reef's first users are coding agents, and no model's training data contains Reef. An agent dropped into a repository with `reef` on `PATH` has to learn the tool from the tool. No human is there to explain it. Design principle 8 says no agent should regex a log. R1.22 gave the CLI JSON on a pipe, exit codes by `ErrorKind`, and `{"error": {"kind", "message"}}`, but before this decision:

- `reef` alone printed clap's usage and exited 2.
- An error said what went wrong and rarely what to do. Where it did, the fix was prose inside the message ("run: reef store build deps ..."), which is exactly the regex the principle forbids.
- An agent inside a workspace had no one-call way to learn which workspace it was in, whether its lease was alive, and what to do next.

R2.05 rewrites the CLI over the typed API, and R2.13 puts an MCP server beside it. Both inherit whatever promises this CLI makes, so the promises need to be written down and tested, not just implemented.

## Decision

We will hold the CLI to five rules. Every later command, including R2.05's rewrite, keeps them:

1. **Never interactive.** No prompt, pager, editor, or confirmation dialog: confirmation is a flag. Only `ws run`'s child reads stdin. Every command completes with stdin closed, and a test runs every leaf command that way.
2. **Every error carries its fix when one is known.** The JSON error object is `{"kind", "message", "hint"?, "next"?}`. `hint` is one sentence. `next` is an argv an agent can run verbatim: it starts with `reef`, has no shell syntax, and has no placeholders. The daemon fills in the ids it knows, so a create after a released workspace is `["reef", "ws", "create", "--repo", "<the real id>"]`. When the fix needs something only the agent knows, there is a `hint` and no `next`. In text mode the same advice follows `error:` on stderr as `hint: ...` and `next: reef ...`, the argv joined with POSIX single quotes where a word needs them. The daemon carries both in `ErrorDetail.hint` and `ErrorDetail.next`. They come from `Advise::advice()` on the error types with a known fix (`ManagerError`, `StoreError`, `LeaseError`, `RegisterError`, `DbError`), read in one place, `reef_daemon::error::status_from`, which is ADR 0017's mapping layer. No handler formats a fix into a message. The CLI adds advice for its own errors: daemon not running, `--timeout` reached, and clap's usage errors, whose hint is the usage line. A stub has none.
3. **The tool explains itself in-band.** `reef` with no arguments and `reef guide` print `docs/agent-guide.md`. The file is embedded with `include_str!`, so the published guide and the binary cannot drift. The whole guide is at most 1,600 bytes. Its short form, everything before `<!-- reef:long -->`, is at most 40 lines, and R1.27 pastes it into `AGENTS.md`. The guide prints as text even on a pipe, because it is prose for a model and JSON escapes only cost tokens. `-o json` returns `{"guide", "commands"}`, where `commands` lists every leaf command's words from clap's tree, so a harness can generate one tool per command. `reef --help` fits in 60 lines, every `about` is one line, and every `long_about` is at most five lines at 100 columns.
4. **One call answers "where am I".** `reef here` finds the workspace by `REEF_WORKSPACE` (set by `ws run`), else by the current directory: the active workspace whose directory is the cwd or contains it, deepest first. It reads that workspace without a snapshot and without walking the directory for disk usage, and prints at most 12 lines or one JSON object: the workspace's fields plus `in_workspace`, `hint`, and `next`. `next` follows the state:
   - healthy and outside `ws run`: `reef ws release <id>`;
   - healthy inside `ws run`: nothing, since `ws run` releases the workspace when its command exits;
   - divergent or flagged: `reef ws inspect <id>`;
   - released, reclaimed, or expired: `reef ws create --repo <id>`.

   Outside every workspace, `here` exits 0 and names the registered repository that contains the directory, if any. Its `next` is `["reef", "ws", "run", "--"]`, the one `next` that is a prefix: the command to append is the agent's own, and the hint says so.
5. **Names are additive.** Every command name in R1.22's surface stays valid for the 0.x line. New names arrive as visible aliases (`workspace` for `ws`, `new` for `ws create`, `ls` for every `list`, `rm` for `ws release`, `agent-guide` for `guide`) and unique prefixes (`infer_subcommands` on the root and every noun). An ambiguous prefix is a usage error, never a guess. `status` is not an alias, because R2.02 owns `reef status`.

Two smaller defaults come with the contract:

- **`REEF_OUTPUT` sets the default for `-o`.** A harness that gives its agent a pty can still get JSON everywhere by setting one variable. `-o` beats it.
- **`ws list --limit N` bounds the one list that grows without bound in Phase 1.** The default is 100 and `0` lists everything. The list shows the newest first and says on stderr when it was cut.

## Consequences

- An agent that runs `reef`, fails, or gets lost is one command away from the next step. RX.07 measures whether that is true in practice.
- Every new error the daemon or the CLI returns is a question: is the fix known? If it is, the fix goes in `advice()`, not the message. `every_next_is_a_real_command` drives the daemon's cases through the binary and checks each `next` against the command tree, so a renamed command breaks a test, not an agent.
- The guide is the most-read text Reef ships to agents. The byte budget is the stopping rule for arguments about what goes in it: something new comes in only when something else goes out.
- `reef guide` embeds a file outside the crate (`../../../../docs/agent-guide.md`). Publishing `reef-vcs` to crates.io would need the file copied into the crate first. Binaries (R1.28) are unaffected.
- `ErrorDetail` grew two fields. Old clients ignore them. A client that reads them from an older daemon gets none and behaves as before.
- `ws list` now lists newest first, where R1.22 listed oldest first. `ws list --limit` does not wrap the JSON array in an object: R1.22 fixed "one array for lists", and changing the shape would break every consumer. Truncation is reported on stderr in both modes instead, so a consumer that reads only stdout does not see that the list was cut. `--limit 0` is the escape.
- A new prefix can make an old prefix ambiguous: `ws re` resolves to `release` today and stops resolving once a `ws rename` exists. The alias tests catch the cases they name. Full names never break.

## Alternatives considered

- **A separate `reef-agent` binary** with an agent-shaped surface. Rejected: two things to discover, two to install, and two surfaces to keep in step, when one CLI that is already non-interactive and machine-readable can carry the contract.
- **Hints in messages only.** Rejected: that is the regex principle 8 forbids, and the messages are for humans and logs. An argv field can be run without parsing.
- **A tokenizer dependency to budget the guide in tokens.** Rejected: build weight for one test, and tied to one model's tokenizer. Four bytes per token is the usual estimate for English and shell text, and RX.07 measures real token counts end to end.
- **`here` exiting 3 outside a workspace.** Rejected: "not in one" is the answer to the question, and an agent that treats any non-zero exit as failure would stop instead of reading `next`.
- **`here` snapshotting first, as `ws inspect` does.** Rejected: `here` must be cheap enough to call before every step, and it must have no side effects. A snapshot records an operation. `--snapshot` opts in.
