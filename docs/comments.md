# Comments and instructions

A comment is a note or an instruction on a change ([ADR 0069](adr/0069-comments-and-instructions.md), R3.12). A **note** is data for whoever reads it. An **instruction** is work for the session that made the change: Reef routes it to that session, shows what it is likely to cost, and hands it over only after someone confirms it. Reef starts no agent. A session takes its instruction through a harness hook, `reef mcp`, `reef comment next`, or the API, and finishes it by proposing the change again.

```sh
reef comment add @- -m "the parser change looks right"                 # a note
reef comment add @- --instruction -m "rename parse_all to parse" \
    --path src/parser.rs --lines 40-52                                 # an instruction, awaiting confirmation
reef comment confirm cmt_8f3a0c2d91e4b675                              # after reading its estimate
reef comment next --wait 10m                                           # in the agent's session
reef propose                                                           # done: the instruction reads DONE
```

## What a comment is

| Field | Meaning |
| --- | --- |
| change, round, commit | The change it is about; its proposal round then (0 for a change never proposed, [proposals](proposals.md)); the commit its author saw (`--expected-commit` checks it) |
| anchor | Optionally a path and a 1-based line range, in the commit's file or, with `--old`, its first parent's (for removed lines). A path that is not there, lines past the end, or lines in a binary file are refused (`INVALID_ANCHOR`). When the change's visible commit has a different file at the path, the comment reads `outdated` |
| reply to | Optionally another comment on the same change |
| kind | `note` or `instruction`, fixed when written |
| text | 1 byte to 16 KiB of UTF-8, with no control characters but newline and tab and no bidirectional overrides or isolates; its SHA-256 is kept with it |
| author | The agent, the actor (`human` or `agent`), the identity, the workspace's provenance session, and the remote token: as [proposals](proposals.md) record them |

Comments are immutable: there is no edit or delete, except forgetting the repository. Correct a note with another that replies to it; cancel an instruction before it runs. A change keeps at most 1,000 comments.

Every write (a comment, and each move of an instruction) is an operation of kind `comment`, whose `reef.comment` attribute lists the events by id and state, never the text. So comments are in `reef op log`, the activity feed, replay and `reef op watch --kind comment`. The text, the state and who did what are in `reef.sqlite3`, which keeps them through GC. A comment's operation cannot be undone (`OPERATION_NOT_UNDOABLE`): a delivered instruction cannot be unsent, and a note cannot be unsaid.

## An instruction's states

| State | Means | Entered by |
| --- | --- | --- |
| `awaiting_confirmation` | Written, with its estimate; nothing delivered | `reef comment add --instruction` |
| `pending` | Confirmed; waiting for its session | `reef comment confirm`, or `add --confirm` within ceilings; `reef comment release` hands a delivered one back (the request names `delivered` as the state it expects, so a confirmation sent again never hands a taken one out twice) |
| `delivered` | A client took it for a session | `reef comment next`, a hook, `reef mcp` |
| `running` | The session said it started | `reef comment start`, or taken with `reef comment next --start`. Hooks and `reef mcp` take it as `delivered`, so one cut off before it reached the model can be released |
| `done` | Finished | `reef comment done`, or the change proposed again after the instruction was delivered (cause `reproposed`) |
| `failed` | Could not be done | `reef comment fail -m WHY`, or running for 7 days untouched (cause `expired`) |
| `cancelled` | Withdrawn or moot | `reef comment cancel` before it runs, the change landing or being superseded (cause `change_final`, for ones not yet delivered), or 7 days untouched before it ran (cause `expired`) |

`done`, `failed` and `cancelled` are final. A cancellation after delivery is recorded and shown, but it stops nothing a session already has. A change has at most 16 unfinished instructions, and a session 64. A change that has landed or was superseded takes notes but not instructions (`CHANGE_FINAL`). The sweep that applies re-proposals and final states also reconciles from the database at start and every ten minutes, so a restart loses none.

## Who an instruction is for

When an instruction is written, Reef resolves its **target**:

1. The session that recorded the change's latest proposal (the workspace `reef propose` ran in), with the client session its harness declared ([provenance](provenance.md)): `source: proposal`.
2. Else the session of the change's latest provenance record: `source: provenance`.
3. Else none: any session working on the change may take it.

The target is a **client session**, the harness's own session id, because that is what resumes; a Reef workspace's session ends when the workspace is released, and the harness's usually outlives it. A claim takes the oldest pending instruction whose target is the caller's client session (from the harness's environment, `--client-session`, or `REEF_CLIENT_SESSION`), or whose target declared none and is the caller's workspace. `reef comment next --take --change REV` takes one meant for another session on that change, and the instruction says `taken`.

Reef cannot tell whether a harness session is still running. `target.last_seen_at` is the latest of the client session's declarations, provenance records and claims, and nothing more.

When a session takes an instruction, the Reef client that took it declares the text's SHA-256 as the instruction of every active workspace of that client session, so the provenance records of the work it does name the instruction ([provenance](provenance.md)). A client of the API does the same with `DeclareProvenance`.

## The estimate

"Estimated cost" is what Reef recorded, and says what it is not:

| Part | From |
| --- | --- |
| checks | The latest finished run of each check ([executor protocol](executor-protocol.md)) on the tree of the change's recorded proposal commit, else its visible commit: how many, and their wall and CPU time. Re-running them after a fix costs about the same. |
| agent time | The median time from delivery to done of earlier instructions that finished `done`: on this change, else to this client session, else the repository's last 50. |
| model, harness | What the target session declared, unverified. |
| unknown | A sentence for each part with nothing to go on, and always "model tokens and price are not visible to Reef". |

The estimate is computed once, when the instruction is written, and stored with its time and digest: it is what the author was shown. It is not a budget; nothing stops a session spending more.

Confirming is a second call, so a person or a program sees the estimate first. `reef comment add --instruction --confirm --max-check-time 10m --max-agent-time 30m` confirms in one call only when the known parts are within the ceilings, and at least one ceiling must be set (`CEILINGS_REQUIRED`); an unknown part meets its ceiling only with `--allow-unknown`. Otherwise the instruction stays awaiting confirmation, and the result says which ceiling stopped it.

## How a session receives an instruction

- **A harness hook.** Reef's harness packages run `reef comment next --hook` at the turn's end, where the harness documents a way for a hook to keep the agent working (it prints that decision, with the framed instruction as its reason), and at a prompt and a session's start (it prints the framed instruction as context). The hook takes the instruction, prints it at once, never fails, never starts a daemon, and gives up after two seconds.
- **`reef mcp`.** `wait_for_operations` with kind `comment` on your change returns, besides the operations, `instructions`: each one for the server's client session that the wait saw confirmed, taken and framed. Reading the log with `list_operations` takes nothing.
- **`reef comment next`.** Takes the next instruction for this session, with `--wait DURATION` following the operation stream until one is confirmed. `--start` starts it.
- **The API.** `CommentService.ClaimInstruction`, which never blocks; wait on `SubscribeOperations` filtered to kind `comment`.

Every one hands the model the same frame:

```text
Reef instruction cmt_8f3a0c2d91e4b675 from alice (human) on change wxmsylkl, round 2, at src/parser.rs:40-52:

rename parse_all to parse

When done, propose the change again (reef propose), or run reef comment fail cmt_8f3a0c2d91e4b675 -m WHY.
```

**When the session has ended**, the instruction stays pending until it is taken, cancelled or expires. Any agent asked to pick it up runs `reef comment next --take --change <change>`. Restarting a session that has ended is a runner's job; R3.46's webhooks will wake one.

## Trust

- On the socket, one uid is one trust domain ([threat model](threat-model.md)): any caller writes, confirms, cancels and takes instructions, and an agent can instruct another agent. The record says who, and as what actor, as declared (S-159).
- Through the remote listener, any `write` token writes notes. An instruction, and a `take`, need a token linked to an operator identity, and are recorded `agent`. Only the token that wrote an instruction confirms or cancels it, only the token that claimed it moves it after, and only the token that proposed the change claims without taking (S-162).
- A note is data. Notes are never claimed, framed or printed by a hook, and an agent reading comments treats a note's text as someone's words, not its instructions (S-161).
- Comment text is as readable as the repository: every `read` token, the web UI and backups see it (S-164).
- Comments are not signed. A person's signature means something once R3.43 verifies presence, and travels once R4.07 publishes changes to a store.

## What later tickets add

- **R3.46**: webhooks that push confirmed instructions to a runner the operator controls, and a runner's report of tokens and price into the estimate.
- **R3.32**: help requests, a comment an agent writes, answered by an instruction that replies to it.
- **R3.31**: comments in the proposal view, with anchors, `outdated`, and the confirm step with its estimate.
- **R3.33**: unanswered help requests and instructions in flight in the triage inbox.
- **R3.20**: compute budgets in policy, which may read stored estimates.
