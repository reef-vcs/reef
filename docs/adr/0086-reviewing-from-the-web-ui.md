# 0086. Reviewing from the web UI: the proposal view's writes and their record

- Status: accepted
- Date: 2026-11-20
- Ticket: R3.31 ([brief](../dev-tickets/phase-3/R3.31-proposal-view.md))
- Amends: [0085](0085-verified-human-presence.md) (decision 7: the allowlist gains four rows; decision 8: the web UI session is recorded on transitions, comments, comment events, landings and operations), [0047](0047-web-ui-transport-and-local-listener.md) (the app gains the Proposals tab and the proposal page)

## Context

The design's human interface is "approve, not read": a reviewer opens a proposal on its intent, then its evidence, then a symbol-level diff that expands to lines, and accepts, declines or comments. Every read it needs was already a READ RPC: `ListProposals` for the entry and, since R3.11, the changes below it (`dependencies`), `EvaluatePolicy` over the stack, `ListEvidence`, `ListComments`, `ListTransitions`, `ListProvenance` and `GetDiff` with symbols.

The writes were not. R3.43 made the web listener's first writes an allowlist ([0085](0085-verified-human-presence.md) decision 7) and gave each web request a caller, `web::Caller { session }`, that a client cannot set. Accept needed nothing more: it is R3.43's one-touch sign-off. A decline (`TransitionChange`), a comment (`CreateComment`), an instruction's confirm or cancel (`UpdateInstruction`) and a landing request (`LandChange`) were refused, and once let through they would have recorded nothing that said a web page asked: their rows record a remote token, and the web caller is not one.

## Decision

1. **Four rows join the allowlist** (`crates/reef-daemon/src/web/rules.rs`):

   | Method | Rule |
   | --- | --- |
   | `ChangeService.TransitionChange` | `decline`: `to` is `DECLINED`, by a person (actor `human` or unset), `expected_commit_id` set, naming no workspace |
   | `CommentService.CreateComment` | `comment`: a note, an instruction or an answer, by a person (actor `human` or unset), not confirmed in the same call, naming no workspace or client session |
   | `CommentService.UpdateInstruction` | `confirm`: a confirm or a cancel, never a release, start or finish, naming no workspace |
   | `LandingService.LandChange` | `land`: by a person, `expected_commit_id` set, naming no workspace |

   A request a row's rule refuses is `WEB_ACCESS_DENIED` with `method` and `rule` in its metadata: no new reason. Through the web, a transition only declines, so a page cannot propose, withdraw or supersede for an agent, and it declines only the commit it showed, since `expected_commit_id` is required (the page sends `expected_state` too). Through the web, an instruction is only confirmed or cancelled: a release, start or finish is the claiming session's, and `comments::update` tells a confirm from a release by the state it moves from. A page's comment is a person's: it never asks for help, which is an agent's, never speaks as a workspace's or client's session, and never confirms in the same call, so an instruction waits for its estimate and Confirm even from a leaked session; an answer to an open help request is confirmed as written, as on the socket, since the agent asked and waits for it, and through the web a reply to a help request already answered waits for its Confirm like any instruction. A web write is a person's: the gate's caller records no `reef-agent` a page sends, and no row names a workspace, so no write records an agent's name or a workspace's session as the person's. The page names a change by its id with the commit it showed as `expected_commit_id`, so an amend after its read refuses the write (`EXPECTED_COMMIT_MISMATCH`). The decline guard, which refuses the remote listener, does not refuse the web. On one uid the web UI is as trusted as the socket ([threat model](../threat-model.md)): any process that can mint a session can call the socket. What the web changes is who can be tricked into a call, which the rows bound and the record shows.

2. **Accept is a sign-off, then a landing request, through two existing rows.** The page signs `signoff`/`pass` on the tree it rendered, with R3.43's approval (one passkey touch), then calls `LandChange` for the same commit with `expected_commit_id`. With `[landing] automatic = true` the request returns the landing the automatic service queued, or queues it a moment sooner; a gate that blocks is a landing with outcome `BLOCKED`, and evidence still running is `waiting`, both shown in words. A sign-off on a stack member counts for the members below it (R3.10), and its landing lands them with it. A request that fails after the sign-off leaves the sign-off, and the page says so.

3. **The web UI session is recorded where a remote token is.** The four writes record a remote token on their rows and, through `caller_of`'s mark, on every operation the call writes. A web call now does the same with its session:
   - `caller_of` marks the agent with the session (`reef_lib::jj::oplog::mark_web`), as it marks a remote token; `oplog::unmark` returns the agent, the remote token and the web session together, and every row writer that read the token reads the session. `OpMeta` writes the operation attribute `reef.web-session`.
   - Migration `0029_web_audit` (schema 27) adds `web_session` to `lifecycle_transitions`, `comments`, `comment_events` and `landings`.
   - The API gains `WebCaller web` on `ChangeTransition` (22), `CommentAuthor` (7), `Landing` (24) and `Operation` (23); the CLI's JSON gains `web_session` on transitions, comment authors, landings and operations.

   The rows are the record that outlives GC; the operation attribute names the session on every operation of the call, the snapshot before it included.

4. **The page holds the commit the person opened.** Every read refreshes on the repository's operations, paced, one in flight. When a refresh finds the proposal at another commit, the page says so and Accept and Decline wait until the person moves to the new version, whose diff it then reads; a comment is written on the commit shown. A sign-off binds one tree, and the page never signs or declines a tree nobody looked at.

5. **The view's order is the design's.** Header and actions (the state, the automatic service's sentence, Accept, Decline, Comment; sticky on a phone), intent (the description as text, the declared provenance, the stack bottom to top with what holds it), evidence (the policy's verdict, each rule, each current object with who signed it and whether a person was verified and pinned), what changed (`GetDiff { symbols, max_call_sites: 0 }`, files by the number of definitions changed, nested by `enclosing_symbol`; a definition expands to its lines and call sites, a file the index cannot answer for to its hunks; expansions read one at a time and kept for the commit), comments (the thread, the estimate and Confirm of an instruction, a help request's Answer box, a note by default, a line anchor from an expanded hunk) and the history, folded. A stack the gate stops below the change shows the change that stops it and its rules.

6. **A Proposals tab lists open proposals until R3.33.** `ListProposals` for `PROPOSED` and `QUEUED`, newest update first, held in place: a proposal that leaves review keeps its row with its new state, and new ones wait behind a count. Proposals are read live: `ListProposals` has no `at_operation`, so the tab and the page say so at an operation instead of reading one.

## Alternatives

- **A web caller as a fifth kind of remote token.** A remote token is a credential with a name, scopes and a revocation; a web session is the daemon's own, short-lived and unnamed. Sharing a column would make every reader tell them apart by shape.
- **Threading the session through each request struct.** The mark already reaches every operation and row writer of a call; a field would reach only the four and leave the snapshots before them unmarked.
- **Accept without a landing request where landing is automatic.** The page cannot tell from the proposal alone whether the service will queue it, and `LandChange` on a queued change returns its landing.
- **Accept up to here from the head's page.** It would sign a member whose diff the page does not show; each member's own page accepts up to it.
- **A `GetProposal` and a stack RPC.** The reads exist, and `dependencies` is the stack.

## Consequences

- A person decides on a proposal from the web UI with one touch, and every web decision is recorded with its session beside where a remote caller's token would be. `reef proposal show`, `reef comment list`, `reef land` and `reef op show` say `web_session` in JSON.
- A leaked session URL can decline, comment, confirm instructions and ask for landings until the daemon restarts, and those writes name the session; it cannot sign (S-258). A landing it asks for onto a destination whose policy pushes is pushed with the user's git credentials, as on the socket.
- The page reads again 10 s after each read while the proposal is in review, since evidence (a check's result, a sign-off) records no operation for the stream to report.
- A proposing agent writes text the page renders: descriptions, paths, comments, a help request's question. All of it is text, never HTML, under the strict CSP (S-257).
- The tab is a placeholder for R3.33's inbox, which opens the same page. R3.23 adds a test-changes section between evidence and the diff. R3.40 lays the page out for a phone and acts through the remote listener once it verifies a person there.
- Workspace, smartlog and feed actions stay off the allowlist, each a row of its own when a ticket needs it.
