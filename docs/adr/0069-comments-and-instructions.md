# 0069. Comments and instructions

- Status: accepted
- Date: 2026-11-09
- Ticket: R3.12

## Context

The [design](../design.md)'s human interface says that "comments are addressed to the originating agent and resume its session, so a review comment is an instruction rather than a note for a human to act on later", warns that this "is the most powerful feature here and the easiest to abuse, since a casual note now spawns work and consumes compute", and asks that "comments are explicitly marked as instructions or as notes, and an instruction shows its estimated cost before it runs". It also says the app "is not an agent runner".

[0063](0063-change-lifecycle.md) records which session proposed a change and made a re-proposal the agent's "ready again". [0058](0058-provenance-records.md) and R3.44 record the harness and its client session a workspace's session declared, and the instruction hash its records carry. `tools/list` is at its byte budget (#116). R3.32's help requests are to be answered "the way an instruction does".

## Decision

1. **A comment is an immutable object on a change**: a note or an instruction, fixed when written; anchored to the change, its proposal round and the commit its author saw, optionally a path, a side and a line range; optionally replying to another comment on the change. Text is 1 byte to 16 KiB with no control characters but newline and tab. Comments attach to any change with a visible commit, not only proposals. There is no edit or delete.
2. **Every write is an operation and rows**, written as [0063](0063-change-lifecycle.md) writes a transition: pending rows (`comments`, `comment_events`, migration 16), an operation with no view change of kind `comment` whose `reef.comment` attribute lists the events by id and state and never the text, then the rows committed; start-up resolution commits or drops pending rows. GC maps the operation ids and deletes nothing. Comment operations are not undoable.
3. **An instruction has a lifecycle**: awaiting confirmation, pending, delivered, running, then done, failed or cancelled. A re-proposal of the change finishes the instructions delivered before it; a landing or a supersede cancels unconfirmed and pending ones; 7 days untouched expires one. A change has at most 16 unfinished, a target session 64.
4. **Routing is by declared client session.** The target is the session of the change's latest proposal, else of its latest provenance record, resolved when the instruction is written; its client session is what a claim matches, since a harness session outlives the Reef workspace session that declared it. Another session may `take` one, which is recorded. Reef reports when the client session was last seen, never that it runs.
5. **Delivery pulls; Reef starts no agent.** One claim RPC, which never blocks, hands an instruction to a session; `reef comment next` (which can wait on the operation stream), harness hooks (at the turn's end where a harness documents a "keep going" output, and at a prompt and a session's start as context), and `reef mcp` inside `wait_for_operations` all call it. MCP gets no new tool: the `comment` kind, and the server takes and frames instructions for its own client session in the wait's result. Every client hands the model the same frame: who wrote it, where, the text, and what to do when done. The client that takes an instruction declares its text's SHA-256 into the session's provenance.
6. **The estimate is what Reef recorded**: the check time of the change's last run on its proposal tree, the median agent time of earlier finished instructions (change, else session, else repository), and the session's declared model and harness; every unknown part is a sentence, always including that tokens and price are not visible to Reef. It is computed once and stored. Confirmation is a second call, or one call within ceilings the known parts meet.
7. **Permissions** are recorded on the socket, as for transitions. Through the remote listener a note needs a `write` token; an instruction and a take need a token linked to an operator identity and are recorded `agent`; confirm and cancel need the writing token, later moves the claiming token, a claim without take the proposing token.
8. **Not signed** until R3.43 (verified presence) and R4.07 (the store) give a person's signature meaning and a place to travel.
9. **`CommentService`** (API revision 27): `CreateComment`, `UpdateInstruction`, `ClaimInstruction` (WRITE), `GetComment`, `ListComments` (READ), in `NOT_TOOLS`; `Operation.comments` and `OPERATION_KIND_COMMENT`. `reef comment` is in `--help-all`.
10. **Webhooks are R3.46's**: push delivery is the daemon's first outbound HTTP, with its own address checks, signing, retries and threat rows.

## Consequences

- [docs/comments.md](../comments.md) documents the object, the states, routing, the estimate, delivery, the frame and trust.
- The threat model gains S-159 to S-165.
- A session that installs no hook, never waits through `reef mcp` and never runs `reef comment next` never sees its instructions; R3.46 adds the push path for sessions that have ended.
- The estimate is thin until instructions finish and jobs run on proposals (R3.45).
- R3.31 renders comments and the confirm step; R3.32 adds help requests answered by replies; R3.33 lists both; R3.20 may budget by the stored estimate.

## Alternatives considered

- **Comments in the commit description, or as jj-visible objects**: they would change commit ids and reach git.
- **Rows only, announced on presence**: no feed, no replay, and an agent could not wait through the operation stream.
- **The text in the operation attribute**: operations grow, and every reader of the log carries it.
- **A token estimate from the text's length and a price table**: numbers Reef cannot see, invented.
- **An MCP tool for instructions**: `tools/list` is at its budget, and a take inside `wait_for_operations` needs none. An MCP resource with `resources/updated` notifications: most clients do not show server notifications to the model.
- **Reef launching a harness to resume a session**: the design's "not an agent runner"; a runner the operator controls does it (R3.46).
- **Signing comments with a key the daemon holds**: it proves only that the daemon wrote the row, which the row says.
- **Routing to the Reef workspace session**: it ends with the workspace, while the harness session usually goes on.
