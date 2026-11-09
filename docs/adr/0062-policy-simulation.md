# 0062. Policy simulation

- Status: accepted
- Date: 2026-11-08
- Ticket: R3.08

## Context

The [design](../design.md)'s human interface gives policy "an editor with simulation that shows which recent landings a proposed rule would have blocked", and [0006](0006-cel-for-policy.md) says "policy simulation evaluates it against recent landings". [0061](0061-repository-policy-file.md) made evaluation a READ call, `EvaluatePolicy`, that takes a policy's text instead of the destination's.

Nothing records a landing yet: the landing service is R3.13, and the `landed` lifecycle state R3.09. Evidence binds to a tree ([0055](0055-evidence-object-format.md)), so a forge's squash merge carries none, and evidence on old trunk commits expires ([0056](0056-evidence-artifact-store-and-retention.md)). One evaluation is bounded (10^7 per expression, 5×10^7 per evaluation); a simulation runs many, and a remote `read` token may send the policy text.

## Decision

1. **Until R3.13 records landings, a landing is a commit that reached the destination**: a non-merge ancestor of `trunk()` other than the root, judged as its own change exactly as `EvaluatePolicy` judges it, with the evidence on its tree and its provenance as they are at the read. The window is a revset, `::trunk() ~ root() ~ merges()` by default, read newest first; any revset is accepted. R3.13's landing records, with the evidence they pinned, replace the default.
2. **Each landing is compared with a baseline**, the policy at `trunk()` by default and the same for every landing, so a report says what a change to the policy changes. Blocked means anything but `allow`. A baseline revision with no policy, or no baseline asked for, compares with nothing (`blocked`/`allowed`), since the landings did land and nothing judged them.
3. **The proposed policy runs as written.** A landing blocked only for evidence that is missing (every unmet rule a `[[require]]` with nothing failed) is flagged and counted apart, since history usually lacks evidence; no option treats missing evidence as present.
4. **A proposed policy that does not load, or a revision without one, is answered at once** with its diagnostics and no landing evaluated.
5. **Every call is bounded**: a page of at most 100 landings (20 by default) that ends after the landing taking its summed cost past 10^8, its touched paths past 200,000 or its evidence and provenance rows past 50,000, or before its landings pass 3 MiB (a landing too large alone is trimmed); at most two pages read at once, each evaluation taking one of the daemon's four evaluation turns, held by the task that runs it; positional page tokens pinned to the first page's operation. The CLI follows pages up to `--limit` (20 by default, at most 1,000).
6. **`PolicyService.SimulatePolicy` is a READ call** (API revision 22) with no storage; `reef policy simulate` reads `./reef-policy.toml` by default and exits 0 whatever it reports.

## Alternatives

- **The destination's first-parent line**: on a merge history, each merge would be judged by its resolution diff alone. `first_ancestors()` remains a revset away.
- **Operations that moved the destination's bookmark**: closer to a landing event, but one `jj git fetch` that moves `main` by thirty commits is one event, and colocated repositories import git's moves.
- **Waiting for R3.13's records**: no simulation until the landing service, though the policy is editable now.
- **The policy in force at each landing as the baseline**: before R3.13 that is no policy for nearly every landing; it answers whether history would have gone otherwise, not what an edit changes.
- **Treating missing evidence as present**: the simulation would no longer match `reef policy eval`.
- **A client-side loop over `EvaluatePolicy`**: a round trip and an evaluation turn per landing, and every client (the editor, the SDKs) would reimplement the comparison.
- **One unpaged call, or a server stream**: an unpaged call is either too small or unbounded; no other read streams like this, which is harder for the web listener and the SDKs.
- **A wall-clock budget per page**: page boundaries would differ between runs.

## Consequences

- [docs/policy.md](../policy.md#simulating-a-policy) documents the window, the baseline, the outcomes, missing evidence and the bounds.
- `SimulatePolicy` is not an MCP tool (`tools/list` is at its budget, #116): a policy author runs `reef policy simulate`; the editor (R3.31, R3.33) calls it.
- The threat model gains S-98 to S-101.
- R3.13 adds a window of landing records, judged with the evidence they pinned; R3.09's `landed` state can select one.
