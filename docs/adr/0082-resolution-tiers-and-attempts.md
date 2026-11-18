# 0082. Resolution tiers and attempt caps

- Status: accepted
- Date: 2026-11-18
- Ticket: R3.28 ([brief](../dev-tickets/phase-3/R3.28-resolution-tiers-and-attempt-caps.md))

## Context

The [design](../design.md)'s Human interface says agents resolve conflicts and people approve resolutions: "Policy sets the tier per area: resolve and land, resolve and require approval, or ask before attempting … Policy caps attempts per conflict so a hard one cannot loop." R3.07 ([0061](0061-repository-policy-file.md)) parses the `[conflicts]` table and looks a tier up with `Policy::conflict_tier`, and nothing called it. R3.27 ([0068](0068-resolver-assignment.md)) assigns each conflict to an agent and queues it when that agent goes, with one seam for this ticket, `may_claim`, which allowed every claim.

Since R3.45 ([0081](0081-automatic-landing.md)), landing needs no request, so a conflict is the main thing that stops an agent working alone. Nothing told the agent that owns one whether it may resolve and land, must get approval, or must ask first, and nothing capped how often a conflict went from resolver to resolver.

## Decision

1. **The daemon looks each assignment's tier up.** The input is built once, in the daemon, from R3.25's conflict object: its paths, and per side the change, identity, agent and actor of a `verified` source (empty otherwise). `GetConflict.policy_input` returns it, and `reef conflict show --policy-input` prints it. The policy is the `reef-policy.toml` at `trunk()`. With no file, the defaults apply (`approve`, 3). An invalid policy or a failed lookup gives `ask`, with the error on the row. The row keeps the tier, the cap, the rule, and the conflict id and policy it came from. It is refreshed when either changes (and a minute after a failed lookup), at most 64 rows a look; new conflicts are always looked up. Under the same policy a narrowing conflict keeps its strictest tier and smallest cap, so a resolver that fixes a strict rule's files first never lifts the requirement; only a person or a new policy unblocks a blocked conflict. A `trunk()` that cannot be read, or a conflict wider than 10,000 paths, is a failed lookup.
2. **An attempt is one resolver's turn.** It starts when a resolver is handed the conflict: its default owner at open or reopen, a claim, an assignment to a workspace, or a person's go-ahead to a live owner. The same agent carrying on (followed, returned) is the same attempt, and a person's takeover is not one. A resolution undone within 24 hours reopens the row and starts another. Declined or failing resolutions are ordinary work, not conflict attempts. `moved`, which every transition calls, counts them.
3. **`BLOCKED` waits for a person.** A hand-off that would start an attempt the tier or the limit does not allow, or put such a conflict in the queue, writes `BLOCKED` instead, with reason `ASK` or `ATTEMPTS` and the intended owner kept. Blocked conflicts are out of the queue. Claims and agents' assignments of them are refused (`ASSIGNMENT_BLOCKED`), and `reef here` says `ask` (`CHANGE_STEP_ASK`). A tier refreshed to allow it hands it to the kept owner, or the queue.
4. **A person's go-ahead.** `AssignConflict` gains `allow` and `attempts`, person only, never over the remote listener. The go-ahead is recorded once per conflict and covers its attempts up to the limit. At the limit it grants one attempt, or `attempts`. A person's assignment to a workspace or the queue is a go-ahead too.
5. **Resolutions need approval unless the tier is `land`.** When an assignment closes `resolved`, `approval` records whether landing needs a person's sign-off. It does when the resolver is not a person and the tier is `approve` or `ask`, or the conflict was blocked when it was resolved. A resolver is a person when the resolving session's actor is `human`, or when the conflict was taken and no agent's session wrote the resolution.
6. **The gate enforces it.** The policy input gains `change.resolutions`. The built-in rule `builtin:resolution` needs one `signoff` on the change's tree from any person when a resolution has `approval`. The gate, `EvaluatePolicy` and simulation count an assignment still open on a change whose judged commit carries no conflict as the resolution its close will record, so the follower's delay never lets one through. Landed history names those approvers as reviewers.
7. **Resolutions are marked on proposals.** `ProposalEntry.resolutions` lists each with its tier, attempts, approval, resolver and conflicted commit; `reef proposal show` prints them.
8. **Retention.** GC keeps a resolution awaiting approval while its change is visible and not landed.
9. **Bounds.** Schema 23 (`0024_resolution_tiers`, columns on `conflict_assignments`; existing rows count their claims and default owner as attempts), API revision 39, S-230 to S-236.

## Consequences

- An agent reads its next step from `reef here`: resolve, or ask a person. With R3.32's help request, asking is one command.
- Reef cannot stop an agent editing a blocked conflict; the cost of doing so is a person's review, not a bad landing.
- A repository with no policy lands nothing through Reef. Its conflicts still get the defaults, so claims stop after three attempts.
- R3.29 writes `ask` with its own reason into the same row; R3.33 and R3.35 list blocked conflicts and resolutions awaiting approval.

## Alternatives

- **Counting only claims that end unresolved** (R3.27's reading). The owner's own turn would be free and unbounded, and a reopened conflict would cost nothing.
- **Counting declined or failing resolutions too.** That bounds a retry loop after landing fails, but it needs resolutions tracked past their close. R3.45 already gives up on failing proposals, and under `approve` a person sees each try.
- **Approvers as a new key on each rule.** That duplicates `[[signoff]]`, which a policy can aim at resolutions through `change.resolutions`.
- **The follower looked at before every gate.** The gate runs where no shared handle to the daemon exists. Reading an open assignment of an unconflicted commit as its pending close gives the same answer without a look.
- **Refusing snapshots of a blocked conflict's files.** That breaks the snapshot model every workspace relies on, and an agent can write the same bytes elsewhere.
- **The daemon raising a help request when a conflict blocks.** It cannot say what the agent tried. The agent asks through R3.32, and `reef here` points it there.
