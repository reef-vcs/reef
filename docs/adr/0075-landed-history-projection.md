# 0075. Landed history projection

- Status: accepted
- Date: 2026-11-13
- Ticket: R3.16 ([brief](../dev-tickets/phase-3/R3.16-landed-history-projection.md))

## Context

The design separates published from working history: "The operation log keeps every agent step. Landed history is a curated projection that policy produces at landing time." Its open problems ask that the projection "squash and summarize by policy without destroying the operation log", and the human interface that "commit messages become agent summaries plus structured provenance."

R3.13 ([0064](0064-landing-service-local.md)) and R3.14 ([0067](0067-batching-and-bisect.md)) land each queued change as its own commit, fast-forwarded or rebased, in one `land` operation, and left the projection to this ticket. Their landings rewrote nothing but parents: an agent's description, empty or not, reached the destination as written, and a change whose edits the destination already had landed as an empty commit. R3.17 will push the destination to a git remote and R4.16 export it, so what lands is what a team on GitHub reads.

A change is already the squash of its working history: every snapshot a Reef workspace takes rewrites the same change, the operation log keeps each step, and the change lands as its last commit.

## Decision

1. **The destination is the projection.** It is written inside R3.13's `land` operation, so the bookmark only ever points at projected commits; there is no second, published bookmark.
2. **One commit per change**, with its change id and the tree that was judged and tested. A batch lands one commit per member, in chain order. Squashing several changes into one is for stacks (R3.10, `squash = "stack"`); a batch is never squashed.
3. **The destination policy's `[history]` table** decides the description, read with `[landing]` at the batch's base: `message = "summary"` (the default inside the table) or `"keep"` (the default without it: commits land as written, so no existing repository's landings change).
4. **A summary is the change's own description**, normalized (`\r\n` to `\n`; control characters but newline and tab, and invisible formatting characters such as bidirectional overrides and zero-width characters, removed; trailing whitespace and outer blank lines removed), with the trailers the projection owns removed from its last paragraph (every `Reef-*` key, and `Reviewed-by` when it writes that set, read as git reads a trailer, whether or not jj reads the paragraph as a trailer block) and its own appended to the trailer block. Other trailers stay, normalized like the rest of the text. Reef writes no prose.
5. **Four trailer sets**, written in this order: `change` (`Reef-Change`), `landing` (`Reef-Landing`, and `Reef-Batch` for a batch that landed several changes), `agents` (`Reef-Agent`, from the change's provenance records, at most 5) and `signoffs` (`Reviewed-by`, the person identities whose sign-offs the landing's evaluation counted, at most 10). The default is `change` and `landing`: ids only, since the destination leaves the machine.
6. **`builtin:description`** blocks a change with no description at the gate (and at landing) when the destination summarizes.
7. **Authorship.** The author is kept; the committer is the daemon's `[identity]` at landing, as for a rebase. The projection never adds `Signed-off-by`: a sign-off certifies the DCO in a person's name. A repository that requires one writes a free rule over `change.description`.
8. **The tree never changes.** The daemon computes each member's trailers; `reef_lib::jj::land` applies the summary to the commit's description in the same rewrite as its rebase (one new commit), and refuses unless each landed tree is the planned one.
9. **Empty landings return.** A change already empty against the destination is blocked at the gate, or dequeued from its batch, with reason `EMPTY`; one that only an earlier member of its batch makes empty is left out and stays queued, so the next batch judges it against what landed and returns it `EMPTY` only once those edits are on the destination. `land_chain` refuses an empty member too.
10. **The working history stays in the operation log.** The land operation records each queued commit as the predecessor of the commit that replaced it, readable until GC drops the operation (`gc.undo_window`); then the landing row keeps the queued and landed commits and trees, `summarized`, and the `[history]` it landed under. No ref keeps queued commits for good. Provenance and evidence stay pinned by the landing rows.
11. **Records and API.** Schema 18 (`landings.history`, `landing_changes.summarized`); API revision 32 (`LandingHistory`, `Landing.history`, `LandedChange.summarized`, `LANDING_REASON_EMPTY`). No RPC, operation kind or MCP tool.

## Consequences

- Under `summary` a fast-forward is a new commit: the commit a published review branch (`reef publish`) shares with the destination differs, and a host will not show that pull request merged. R3.17 pushes the destination itself.
- Every summarized landing rewrites its change, so changes proposed on top of it are rebased in the same operation (trees unchanged, evidence still applies) and read `amended` until proposed again; R3.11 carries them.
- Trailer keys are a public format from R3.17 on.
- An agent's `Reef-*` trailers never reach a summarized destination; the threat model's S-191 to S-195 cover forged and leaking trailers, a projection that changes content, injection through descriptions, and working history after GC.

## Alternatives considered

- **A separate published bookmark** beside an unprojected one: agents would build on one and git users on the other, and every fetch would conflict them.
- **Squashing a batch into one commit**: hides every member's change id (which the history index reads as abandoned), mixes unrelated work, and loses per-change bisect and revert.
- **A message template or a CEL expression** for the message: more surface for no clear need; a free rule can require a title format.
- **Summary by default for every policy**: changes every existing repository's commits without a reviewed policy change.
- **Keeping queued commits with `refs/reef/…` refs**: disk for good, and `git log --all` in a colocated repository would show them.
- **Setting the author from the proposer**, or **adding the operator's `Signed-off-by`**: the first matters only for daemons serving several people (a follow-up); the second would certify the DCO for someone who did not.
