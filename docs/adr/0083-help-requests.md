# 0083. Help requests from agents

- Status: accepted
- Date: 2026-11-19
- Ticket: R3.32 ([brief](../dev-tickets/phase-3/R3.32-help-requests.md))

## Context

Since automatic landing ([0081](0081-automatic-landing.md)), an agent can propose, collect evidence and land with nobody asking. It still has no way to ask a person. An agent that meets a product decision, a conflict it may not resolve, a missing credential or an ambiguous task either guesses, or ends its turn with a question in a transcript nobody running a fleet reads. The design puts "an agent asking for help" in the triage inbox's first bucket.

Comments ([0069](0069-comments-and-instructions.md)) already deliver a person's words to the session that made a change, by pull, through `reef comment next`, harness hooks and `reef mcp`'s `wait_for_operations`. Their `CommentKind` and `reply_to` were left as the seam for this ticket.

## Decision

1. **A help request is a comment kind.** `COMMENT_KIND_HELP_REQUEST` on a change, with a reason (`decision`, `conflict`, `blocked`, `unclear`, `other`), the text of what is wrong, what the agent tried (required), an optional question, and the asking client session (the request's, else its workspace session's). It is recorded like every comment: rows in `reef.sqlite3` (migration `0025_help_requests`, columns on `comments` and `comment_events`) and an empty operation whose `reef.comment` attribute carries ids and states, never text. It is never claimed or delivered to a session.
2. **A conflict is named through its change.** Reason `conflict` requires the commit to be conflicted, and records R3.25's conflict id and R3.27's open assignment id; a request may name either id. `conflict_moved` is computed on read.
3. **States.** `open`, `answered` (in the operation of the first answer), `withdrawn` (by a client, or the sweep's `change_final` and `expired` after 7 days). Cancelling every answer before one was delivered opens it again (`answer_cancelled`). Operations are not undoable.
4. **The answer is an instruction that replies to it.** It goes to the session that asked (target source `help_request`), is written `PENDING` with its estimate stored (the confirmation step protects a person from work nobody asked for; here the agent asked), and is refused from the asking session (`SELF_ANSWER`). Delivery is R3.12's claim, unchanged; the clients frame it as an answer.
5. **What waits.** A help request the workspace's own session asked, open or with an untaken answer, sets `CHANGE_STEP_AWAIT_HELP`, which outranks every step but `done`, with `Workspace.help_request_id`; another session's help request leaves the step alone. An open help request holds automatic landing of its change's stack (`needs`, reason `help`) unless a member is already queued; asking, answering, withdrawing, reopening and expiry mark the change. An explicit `LandChange` is not held.
6. **Surface.** `CreateComment` with `help` and `client_session`; `WithdrawHelpRequest`; `ListCommentsRequest.help_states`; `reef ask [--wait]`, `reef comment answer|withdraw`; an MCP tool, `ask_for_help`, on the server's own workspace, raising the `tools/list` bound to 25,900 bytes. Remotely, any `write` token asks, naming no session (its `client_session` is ignored); an answer needs an identity-linked token; only the asking token withdraws and claims the answer.

## Alternatives

- **A separate object and table.** A second write path, record and claim for what a comment kind and a reply already carry.
- **Answers confirmed in a second call.** The agent waits on a person, and the person would then wait on a confirmation.
- **A structured verdict on answers** (approve, stop). What an answer allows for a conflict is R3.28's call; text reaches the agent either way.
- **The daemon raising help requests** when a resolution tier blocks. It cannot say what was tried.
- **Waiting inside `ask_for_help`.** Calls are capped at 300 seconds, and `wait_for_operations` already returns the answer inline.
- **A turn-end hook that keeps a waiting session open.** It holds a model turn while a person may take hours.

## Consequences

- R3.33 lists open help requests as "needs you now"; R3.28 points its blocked tiers at `reef ask --reason conflict`; R3.46 pushes answers like any instruction.
- An agent can answer another agent's help request on the socket, as declared (one uid is one trust domain); remote answers need a linked identity.
- A help request on another agent's proposal holds its automatic landing until it is answered, withdrawn or expires.
