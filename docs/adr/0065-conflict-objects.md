# 0065. Conflict objects

- Status: accepted
- Date: 2026-11-08
- Ticket: R3.25

## Context

The [design](../design.md) makes conflicts first-class: "A merge that conflicts still produces a valid snapshot containing the conflict. It can be stored, pushed, handed to a resolver agent, or fixed later," and "the API exposes it as data." A resolver "sees base, both sides, and the task and instruction behind each." Five later tickets read conflicts: agent-scoped undo (R3.26) and dependents (R3.11) attach conflicts of their own, resolver assignment (R3.27) routes them, resolution tiers (R3.28) look them up in [0061](0061-repository-policy-file.md)'s policy, the conflict view (R3.35) renders them, and the merge engine (R5.01) returns one when a merge does not resolve.

jj-lib already stores the conflict. A conflicted commit's tree is a `Merge<TreeId>` of *n* sides and *n − 1* bases, interleaved side, base, side, …, with one label per term. A label is free text: jj writes `"<change:8> <commit:8> \"<subject>\" (<role>)"`, the role saying `rebase destination`, `rebased revision` and so on, but older conflicts have none and any client can write any. jj's tree merge writes every entry it can resolve into every term, so a term's root tree names no commit; a conflicted path's value in a term is the source commit's value there. Hunks are not stored; jj computes them when it writes markers.

Before this, Reef reported only that a conflict existed (`Change.conflict`, `GetStatus`'s conflicted paths, `ReadFile`'s markers). [0058](0058-provenance-records.md)'s provenance records are found by change, and most commits (a rebase's, a describe's) have none of their own.

## Decision

1. **A conflict object is one conflicted commit's stored conflict, derived from jj on every read and never stored.** It reports every term with its role (side or base), index and jj's label, and every conflicted path with each term's value (type, object id, executable bit), a kind (`content`, `executable`, `add-add`, `modify-delete`, `type`, `other`), and whether a parent's own tree carries the same conflict at that path (`inherited`). A conflict of more than 64 terms reports paths only. The Rust reader takes a `MergedTree`, not a commit, so R5.01 can return the same object for a merge it has not written.
2. **Ids follow the conflicted content.** A file's id is the SHA-256 of its path and its simplified terms (jj's simplification, which drops repeated side and base pairs and is deterministic). The conflict's id is `cfl_` and 32 hex digits of the SHA-256 over the change id and its files' ids in path order. Labels are not hashed. A describe, an edit to a path that does not conflict, or another path's conflict widening leaves both ids; a change to a conflicted path's sides changes them; a child that inherits a conflict shares its file ids, not its conflict id.
3. **A label proposes a term's source; content and ancestry decide.** The linked commits are the conflicted commit's parents, up to 16 predecessors of its change (found within 1,000 operations) and their parents; a commit a label names (resolved by the commit id prefix in the index, hidden commits included, kept only when the change id prefix matches) is linked when it is one of those or an ancestor of a parent. A term's source is the first linked candidate, its own label's first, whose tree holds exactly the term's value at every conflicted path checked (the first 200), when the term holds a value at one of them at least: `verified`. Otherwise a commit its label names is the source as `label`, declared; otherwise `none`. `verified` says a related commit holds that content, not that it was the one merged; an unrelated commit, or a side that holds nothing, never verifies.
4. **Each verified side's provenance is its source change's.** For a `verified` side, the daemon reads the newest 200 provenance records of the source's change, re-verifies them as every provenance read does, and reports the sessions that recorded it (at most 10, each with its count of this change's records), the newest record that verifies at or before the source commit, the exact commit's record when it has one, and the counts; at most 64 changes a response. The conflicted change gets the same summary. `label` sides and bases get a source but no provenance, and `reef conflict show --policy-input` gives a side's identity and agent only when it is `verified`.
5. **Hunks are computed on request, bounded.** `GetConflict` with `hunks` splits each content or add/add file with jj's merge options (line hunks, the same split jj's markers show), in file order, resolved hunks with their text and conflicted ones with each term's text and line numbers in the rebuilt term. A file with a NUL in a term's first 8,000 bytes is `binary`, and one with a term over `max_file_bytes` (1 MiB by default, at most 16 MiB), terms over 32 MiB together, hunks over 2 MiB, or alone over the response's limit is `too_large`; neither is split. A page ends after the file whose hunks pass 2 MiB in all.
6. **`ConflictService` is read-only.** `ListConflicts` (a revset in its log order, `mutable() & conflicts()` by default, each conflict with its first 20 paths, at most 10,000 commits looked at a page) and `GetConflict` (one revision, its files a page at a time), both READ, API revision 24. `ConflictKind` is `TREE` only; R3.26 and R3.11 add the kinds jj does not store, with their own storage. `reef conflict list` and `show` (`--hunks`, `--policy-input` for `reef policy check --input`) are in `--help-all`; the MCP tools wait for R3.27.

## Consequences

- No migration and no write path: a conflict read is as fresh as the operation it reads, and needs no reconciliation after a rewrite, undo, GC or restore.
- A forged label cannot make an unrelated commit's provenance read as a side's `verified` source. A commit related to the conflicted one (an ancestor of a parent, say) with the same content at the conflicted paths can still verify.
- A rebased side's source is often a hidden commit; it stays readable while jj indexes it, and falls back to its label once operation GC has dropped the evolution behind it.
- Each read walks every conflicted path of the commit (for the ids, the counts and `inherited`, which checks each parent's own tree, so a merge's conflict is its own), and checks candidates' trees at up to 200 of them. That walk is not bounded beyond the tree itself.
- R3.27 and R3.28 keep the state only Reef knows (assignee, attempts) in rows keyed by conflict or file id, and compare ids before acting.

## Alternatives considered

- **One object per path.** R3.07's `ConflictInput`, a resolver, and `Change.conflict` all work per change; per-file ids serve what needs a finer key.
- **A `conflicts` table filled when a conflict is first seen.** A copy of jj's state that every rewrite, undo and GC would have to follow.
- **Trusting labels.** Free text any client writes (S-127).
- **Attributing by parents only.** Misses rebases, the common case: their sides are a parent and the change's own earlier commit.
- **Matching a term's root tree to a commit's.** jj's merge writes resolved entries into every term, so no commit has that tree.
- **Recording sources when Reef writes a conflict.** The jj CLI writes most conflicts, and R3.13 and R5.01 can call the same reader.
- **Embedding every provenance record.** Unbounded; `ListProvenance` pages them.
- **Hunks always.** Large responses for callers that only route or count.
- **An MCP tool now.** `tools/list` is at its budget, and R3.27 is where an agent is handed a conflict.
