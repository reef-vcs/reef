# Issue #262: `reef here` names the next step of the change's lifecycle

Phase 3 · Size M, low end · Fixes: issue #262, issue #327 (folded in), and `reef here`'s part of #123 · Depends on: R1.26 (`reef here`, ADR 0025), R2.23 (the status index, ADR 0048), #114 (brief JSON, ADR 0045), R3.09 (lifecycle states, ADR 0063), R3.10 (stacks: `reef propose` takes the stack), R3.13 (`reef land`), #258 (`reef publish`), #259 (`behind_trunk`, `reef rebase`), R3.25 (`reef conflict show`), #263 (overlaps in `here`) · Builds on: ADR 0031 (writes hold workspaces still), ADR 0051 (task-first help) · Interacts with: R3.45 (automatic landing, in flight: a proposed change may land without `reef land`; see "What later tickets inherit") · Unblocks: nothing listed

## Context

Agents are Reef's users, and `reef init` sets them up with one instruction: run `reef here` to learn where you are and what to do next. Today `here` stops at getting into a workspace. Its `next` is `reef ws release`, `reef ws renew`, `reef rebase` or a divergence listing, and its hint always ends "`reef publish` sends its change to a git branch for review." The guide (`docs/agent-guide.md`, what `reef init` writes into `AGENTS.md` and the skills) names `describe`, `rebase` and `publish` but not `propose`, `land` or `conflict`, and `reef --help` leaves them out of its "Also" line. An agent set up by `reef init` learns how to start work, not how to finish it, although propose, stacks (R3.10), land, landing push (R3.17) and conflict objects (R3.25) all exist.

The issue asks for three things: `here`'s `hint` and `next` follow the change's lifecycle, suggesting `propose` and `land` only in a repository that uses them; a brief view for `here` (#123); and one guide line for the finishing path within ADR 0025's byte budget. Acceptance: a test per state asserting `next`; the guide's limits hold or the ADR is amended; the token benchmark's `state-own` question does not get more expensive.

#327 is folded in because `here` will tell agents to run `reef describe -m`: a describe run right after a write described the change without that write.

### What was verified while writing this ticket

Reef at `f72d0cd7`: API revision 36, schema 21, ADR 0079 the latest, threat rows to S-215. R3.45 runs in parallel with revision 38, schema 23, ADR 0081 and S-221 to S-229. This brief holds **API revision 37**, **schema 22**, **ADR 0080** and **S-216 to S-220**, and uses **API revision 37**, **ADR 0080** and **S-216** ("Answers applied"): no migration.

- **#327's cause** (reproduced 3 of 3 on the installed daemon at `f72d0cd7`, in a throwaway workspace): a file written, then `reef describe -m` at once; the commit the describe returned did not have the file each time, and had the files of the earlier rounds. The workspace converged a second or two later, when the watcher's snapshot added the file to a new commit of the same change. The cause is in two places that agree with each other: `DescribeChange`'s contract says "Never snapshots, as it changes no tree" (`changes.proto`), and `Manager::rewrite` step 3 skips the snapshot for `Write::Describe` (`workspace/rewrite.rs`). The service, `ChangeHandler::describe_change` (`service/changes.rs`), has no `uses_working_copy` + `snapshot_first` step, which `TransitionChange` (`reef propose`) and `PublishChange` (`reef publish -m`) both have. So it is not the watcher's quiet window racing a snapshot: no snapshot runs at all, and whatever the watcher has not yet recorded (its debounce) is missing from the described commit. The watcher's events are kept while the rewrite pauses it (`Scheduler::pause`), so nothing is lost on disk; what is wrong is the commit the describe names and returns, which a script then proposes, publishes or passes as `--expected-commit`.
- **The cost of the fix.** `snapshot_first` goes through the status index (R2.23): with nothing changed since the last scan it takes no snapshot; otherwise it snapshots the changed paths alone. Measured on this repository (M2 Max, warm daemon, idle workspace, CLI wall time, median of 15): `reef status` 12.2 ms, `reef status --no-snapshot` 10.1 ms, so the index sync costs about 2 ms when clean. An explicit snapshot after one write: 120 to 160 ms (a walk), which the index avoids.
- **`reef here` today** (`reef-vcs/src/cli/here.rs`): `GetWorkspace` (no snapshot, `skip_disk_usage`) and `ListPushes`, then `advise_with_disk` picks `hint` and `next` by the workspace's health: divergent, attention, lease in grace, disk low, behind trunk, missing dependencies, disk warn, then "work here". It reads nothing about the change. Its piped output is the full record: 1,543 bytes brief (no brief view exists), 1,860 with `-o json`. Its text is at most 12 lines (`text_fits_twelve_lines`). Median wall time 7.1 ms; the CLI's own floor (`reef --version`) is 5.5 ms.
- **What the change's state needs, and where it is.** `GetStatus` for a workspace already returns, in one read synced through the index: the working-copy `Change` (`description`, `empty`, `conflict`, `divergent`, `immutable`) with its `lifecycle` at head (`state`, `recorded`, `amended`, `destination`, `last`), its `parents` with theirs, `changed_file_count`, `conflicted_path_count`, and the repository's `trunk_bookmark` (`service/status.rs` fills the lifecycle of the working copy and its parents). `GetWorkspace` does not carry the change. `GetStatus` also reads repository-wide parts `here` does not need (trunk, `heads(mutable())`, active workspaces): 79 KB of JSON on this repository.
- **Whether a repository uses proposals.** Nothing lands without a policy: a destination with no `reef-policy.toml` lands nothing (`docs/landing.md`; `PolicyDecision::NO_POLICY`). This repository has `reef-checks.toml` and no `reef-policy.toml`, and no recorded proposal (`reef proposal list --state …` is empty). A `ReadFile` of `reef-policy.toml` at `trunk()` answers NOT_FOUND in 7.9 ms wall (2.4 ms over the floor); `ListProposals` with every state and `page_size` 1 in 7.3 ms (1.8 ms over).
- **The guide's budget.** `docs/agent-guide.md` is 1,760 bytes against the test's 1,800; its long part 1,741 against 1,750 (`guide::tests::guide_fits_its_budget`, ADR 0071's amendment of ADR 0025). The task-first help (`cli/help.txt`) is 511 of its 512 bytes (ADR 0051, `cli.rs`'s golden test). Something new comes in only when something else goes out (ADR 0025).
- **The commands `next` will name exist with these defaults**: `reef describe -m <text>` (`@`), `reef propose` (`@` and its stack, for the trunk bookmark), `reef land` (`@`, waits up to 2 minutes, reports a block with the policy's reasons), `reef conflict show` (the workspace's change), `reef proposal show` (`@`), `reef op watch --kind transition --change <id> --follow`, `reef publish` (`@`, needs a git remote). Each resolves the workspace from `REEF_WORKSPACE`, else the current directory, as `here` does (`scope.rs`), so `next` needs no `--workspace`.

## Decisions

### Size (decision 1)

**M, low end, and no API change.** `here` composes reads that exist; the daemon changes only in `describe_change` (#327). Out: lifecycle fields in the daemon's API for MCP, the SDKs and the web UI (decision 9 and the user's decision 1), policy evaluation inside `here` (user's decision 3), `reef guide <topic>` pages (user's decision 5).

### The data shape (decision 2)

`here` reads four facts about the change and one about the repository, and computes one value from them, the step. Everything else (`hint`, `next`, text) is rendered from the step and the workspace's health.

```rust
/// What `here` knows about the workspace's change, from one `GetStatus`
/// (#262). `None` when the status could not be read (a workspace that is
/// not active, an older daemon): then `here` advises as before.
struct Change<'a> {
    working_copy: &'a json::Change,      // description, empty, conflict, lifecycle
    changed_file_count: u32,
    conflicted_path_count: u32,
}

/// How the repository finishes work.
enum Track {
    /// Its trunk has `reef-policy.toml`, or a change of it was ever
    /// proposed: `reef propose`, then `reef land`.
    Proposals,
    /// Neither: `reef publish` to a git branch, or release.
    Publish,
}

/// The change's next step in its lifecycle (`docs/proposals.md`),
/// serialized lowercase as `step`.
enum Step {
    Work,     // nothing changed yet
    Resolve,  // the change has conflicts
    Describe, // files changed, no description
    Propose,  // described draft, withdrawn, or a proposal amended since
    Land,     // proposed at its current commit
    Wait,     // queued: the landing service has it
    Revise,   // declined
    Publish,  // described, and the repository does not use proposals
    Done,     // landed or superseded
}

fn step(change: &Change, track: Track) -> Step; // pure
```

`step` is the change's lifecycle alone; it never says "renew" or "rebase". `hint` and `next` put the workspace's health first and the step after (decision 4). Keeping the two apart is what lets an agent or a harness branch on `step` without parsing prose, and what makes one test per state a table.

| Facts (first match) | `step` |
| --- | --- |
| `conflict` or `conflicted_path_count > 0` | `resolve` |
| lifecycle `queued` | `wait` |
| lifecycle `proposed`, `amended` | `propose` |
| lifecycle `proposed` | `land` |
| lifecycle `declined` | `revise` |
| lifecycle `landed` or `superseded`, or `immutable` | `done` |
| `empty` | `work` |
| description blank (after trimming) | `describe` |
| track `proposals` (draft or withdrawn) | `propose` |
| otherwise | `publish` |

**JSON.** `cli.Inside` (the full form) gains `step`, `conflicted_path_count` (left out at 0), and `working_copy` (the full `json::Change`, as `reef status` prints it; left out when the status could not be read), and fills the flattened `changed_file_count` it already declares (today always `null` in `here`). Nothing is renamed.

**The brief view** (#123's part for `here`, ADR 0045's three rules), a new `brief::Here`: always `in_workspace`, `id`, `path`, `change_id_reverse_hex`, `changed_file_count`, `step` and `hint`; `next` when not empty; `state` when not `active`; `held`, `divergent`, `attention` and `conflicted_path_count` when set; `disk`, `disk_status`, `push`, `overlaps` and `overlap_count` when present, as their own views. Dropped: `repo_id`, `owner`, `purpose`, `change_id`, `commit_id`, `backend`, `created_at`, `last_snapshot_at`, `last_snapshot_op`, `lease`, `watcher`, `watch_state`, `local_snapshot_count`, `stores`, `provenance`, `behind_trunk` (the hint says it), `working_copy` (`step` says what an agent acts on; `reef status` and `reef show` have the change). `path` is beyond the issue's list because "where am I" is `here`'s question, and `REEF_WORKSPACE` can name a workspace from another directory. Expected size: about 330 bytes for an idle workspace, against 1,543 today. `Outside` is already small and keeps its form.

**Text.** The existing `change` line gains the step's facts, so the 12-line bound holds: `change:        wswuvlsupqlz, 3 files changed, proposed for main (amended)`.

### Where the facts come from (decision 3)

**From reads that exist, issued at once.** `here` sends `GetWorkspace` (as today), `GetStatus` (`max_files` 1, synced through the index), `ListPushes` (as today), `ListProposals` (every state, `page_size` 1) and a `ReadFile` of `reef-policy.toml` at `trunk()`, concurrently, so its latency is the slowest of them, not their sum. With `--snapshot`, `GetWorkspace` snapshots first and `GetStatus` follows it with `skip_snapshot`, so the two do not race. A failed `GetStatus` (a workspace not active) leaves the change unknown and `here` advises by health alone, as today; a failed probe counts as "no".

`here` now syncs the status index, so it may record a snapshot of the paths written since the last one, as `reef status` does. R1.26 made `here` a read with no snapshot; the step depends on whether files changed, and an agent runs `here` right after writing, so the last snapshot would say `work` to an agent that just wrote. The index costs about 2 ms when nothing changed (measured above). This is recorded in ADR 0080.

**Track** is `proposals` when the `ReadFile` finds the file, `ListProposals` returns an entry, or the working copy's lifecycle is recorded; `publish` otherwise. It is read at `trunk()`, the default destination; a change proposed `--to` another bookmark is recorded and so reads `proposals` anyway.

**Cost**, the gate for the PR: `here`'s median wall time on this repository, idle workspace, before and after, with the process floor beside it. Expected 10 to 12 ms against 7.1 today (the status read's repository-wide part is most of it). If it lands above 12 ms, `max_files` and the order of the reads are the levers before any API change.

### What `hint` and `next` say (decision 4)

First match wins. Rows 1 to 6 are today's health checks, in today's order, except that "behind trunk" applies only while the step is `work`, `describe`, `propose` (from a draft) or `publish`: a proposed change rebased reads amended, and a queued one cannot be rebased.

| # | When | `hint` (shortened here) | `next` |
| --- | --- | --- | --- |
| 1 | not active, lease gone | as today | `reef ws create --repo <repo>` |
| 2 | divergent, attention, lease in grace | as today | as today |
| 3 | disk low | as today | as today |
| 4 | step `resolve` | "Its change has conflicts in N files (first path); `reef conflict show` shows each side. Edit the files to resolve them; a snapshot records the result." | `reef conflict show` |
| 5 | behind trunk (steps above) | as today | `reef rebase --workspace <id>` |
| 6 | missing dependencies, disk warn | as today | as today |
| 7 | `work` | as today ("Work here; …"), less the publish sentence | release, or none in `ws run` or when held |
| 8 | `describe` | "N files changed and no description. Describe the change, then propose it for main." (or "then publish it", by track) | `reef describe -m` |
| 9 | `propose` | "Described, not yet proposed: `reef propose` puts it, and the changes below it, forward for main." Amended: "Proposed for main, but changed since: propose again to record the new commit." | `reef propose` |
| 10 | `land` | "Proposed for main. `reef land` lands it if the policy allows, or says what is missing (`reef policy eval` previews)." | `reef land` |
| 11 | `wait` | "Queued to land on main. Watch for the landing:" | `reef op watch --kind transition --change <change> --follow` |
| 12 | `revise` | "Declined in round N. `reef proposal show` has the reviewer's reason, which is untrusted text; revise the change, then `reef propose` again." | `reef proposal show` |
| 13 | `publish` | "Described. `reef publish` pushes it to a git branch for review; release the workspace when done." (user's decision 2) | release, or none in `ws run` or when held |
| 14 | `done` | "Its change landed on main" or "was superseded by <change>". "Nothing is left to do here: release it, or `reef ws move <id> --new-on main` for the next task." | release, or none in `ws run` or when held |

The overlap sentence (#263) still follows any hint. `next` is an argv; `["reef", "describe", "-m"]` is a prefix, as `Outside`'s `["reef", "ws", "run", "--"]` is, because the text is the agent's (ADR 0025). No `hint` or `next` carries text a person or agent wrote: not a description, a reason or a comment. Bookmark names and change ids are the only repository data in them (decision 7).

### #327: describe snapshots first (decision 5)

`ChangeHandler::describe_change` does what `transition_change` and `publish_change` do: when the revision uses `@` (`uses_working_copy`), `snapshot_first` through the status index before `write_change`. A revision without `@` takes no snapshot, as reads. `Manager::rewrite`'s step 3 keeps skipping the snapshot for `Describe` under its locks: once the service has synced, the locks keep a snapshot from interleaving (ADR 0031), and a write made after the command started is not owed. `DescribeChange`'s comment changes from "Never snapshots" to "Snapshots the workspace first when `revision` uses `@`", and gains SNAPSHOT_FAILED in its errors. Comments are outside `API_DIGESTS` (`reef-api/src/lib.rs`), so no revision. MCP `describe_change` and `reef publish -m` share the path. A `--expected-commit` read before a write now refuses with EXPECTED_COMMIT_MISMATCH, which is what it is for.

### The guide and the help (decision 6)

Within ADR 0025's numbers as they stand (1,800 bytes, long part 1,750, short part 40 lines), with the user's decision 5:

- **Short part** (what `reef init` pastes into `AGENTS.md`): `reef here                  # where am I, and the next step to finish`. The short part is the only text every agent set up by `reef init` reads, and `here` now carries the path.
- **Long part, "Changes"**, rewritten as one paragraph naming the finishing path: `reef here` names the next step; `describe -m <text>`; `propose` and `land` where a policy lands changes, else `publish -m <text>` to a git branch; `rebase` onto trunk; `conflict show`. The bytes come out of the same section (the read commands' list) and the "Commands" section's wording; the budget test is the gate.
- **`reef --help`'s "Also" line** gains `propose land conflict` within 512 bytes, by dropping three of the least-used names (`remote repo config`, all still in `--help-all`).
- Regenerate the agent files and harness packages (`reef init`, `just packages`), which `just preflight` checks.

### Threat row and ADR (decision 7)

**ADR 0080, "`reef here` walks the change's lifecycle"**: decisions 2 to 6, and the track rule. ADR 0025 gets a line saying `here` syncs the status index (R1.26 said no snapshot); ADR 0031 a line saying a describe of `@` now syncs before its write. **S-216**, injection: `hint` and `next` are what an agent reads as advice and runs. A decline reason, a description or a comment in them would put another party's text into an agent's instructions. Fixed: they are built from Reef's own sentences, ids, counts and bookmark names; a declined change's `next` points at `reef proposal show`, which marks the reason untrusted (`docs/proposals.md`). Tests: `here::tests::hint_never_carries_untrusted_text`.

### MCP (decision 8)

One sentence in the MCP server's instructions names the finishing tools, so an agent on MCP learns the same path (user's decision 6): "To finish, describe_change, then propose_change and land_change where the repository has a policy, else publish_change." `tools/list` is unchanged.

### Not in the daemon (decision 9)

`step` is computed in `reef-vcs`, from data the daemon already serves. The daemon's API gains nothing, so MCP, the SDKs and the web UI read the same facts (`get_status`) and decide for themselves. Moving the computation into the daemon is the user's decision 1's alternative.

## Deliverables

### `crates/reef-daemon/`

- `service/changes.rs`: `describe_change` snapshots first when the revision uses `@` (decision 5).
- `workspace/rewrite.rs`: module docs, step 3: why describe takes no snapshot under the locks.

### `crates/reef-api/`

- `changes.proto`: `DescribeChange`'s comment (decision 5). No field, no revision.

### `crates/reef-client/`

- `brief.rs`: `Here` (decision 2), with unit tests of the keys kept and dropped.

### `crates/reef-vcs/`

- `cli/here.rs`: the concurrent reads (decision 3); `Change`, `Track`, `Step`, `step()`; `advise` reworked around the step (decision 4); `Inside` gains `step`, `conflicted_path_count`, `working_copy`; text `change` line; brief output through `brief::Here`.
- `cli/help.txt`: the "Also" line (decision 6).
- `cli/schema/outputs.rs`: `here`'s row covers the new keys and the brief view.
- `cli/mcp/…`: the instructions sentence (decision 8).

### Documentation

- `docs/agent-guide.md` (decision 6); `AGENTS.md` and the harness packages regenerated.
- `docs/adr/0080-here-walks-the-lifecycle.md`, the ADR index; one-line amendments to ADR 0025 and ADR 0031.
- `docs/proposals.md`: "Following a proposal" says `reef here` names the next step. `docs/quickstart.md` if it lists `here`'s output.
- `docs/threat-model.md`: S-216.
- `README.md` "Phase 3 so far", `CHANGELOG.md` Unreleased.
- This brief's README row.

## Tests

`crates/reef-vcs/src/cli/here.rs` unit tests (pure, no daemon):

1. **One test per step** (the acceptance): a table of facts to `step`, and for each row of decision 4 the `next` argv, in a plain workspace, in `ws run`, and held. Includes amended, withdrawn (proposes again), declined, queued, superseded, landed, and a change with files but a blank description.
2. **Precedence**: divergence, a lease in grace and a low disk outrank every step; conflicts outrank behind trunk; behind trunk does not apply to `land`, `wait`, `revise` and `done`.
3. **Track**: `describe` and a described draft say propose with a policy or a recorded proposal, publish without.
4. **No untrusted text** (S-216): a change whose description and decline reason are marker strings; neither appears in `hint` or `next` for any step.
5. **Unknown change**: no status (an inactive workspace) advises as today.
6. **Text** still fits 12 lines with the new `change` line.

`crates/reef-client/src/brief.rs`: `Here` keeps and drops what decision 2 lists; zero values left out; an idle workspace's brief `here` under 400 bytes.

`crates/reef-vcs/tests/cli.rs` (through a test daemon), the path end to end:

7. **No policy**: create, write, `here` says `describe`; describe, `here` says `publish` and `next` is release.
8. **With a policy** (the landing tests' fixture, a `reef-policy.toml` at trunk that allows): write, `here` says `describe`; describe, `propose`; propose, `land`; land, `work` on the new empty change the landing gave the workspace. A withdrawn change says `propose`; a declined one `revise`.
9. **Conflict**: a workspace whose change conflicts with trunk after `reef rebase`: `resolve`, `next` `reef conflict show`.
10. **Fresh after a write**: write a file and run `here` at once with automatic snapshots off (`testing::no_automatic_snapshots`): `changed_file_count` 1, step `describe`.

`crates/reef-daemon/tests/writes.rs` (#327):

11. **Describe right after a write**, automatic snapshots off: write `a.txt`, `DescribeChange(@)` at once; the response's commit has `a.txt` (`ReadFile` at that commit id), the change is not divergent, and the description is on it. The same test fails on `f72d0cd7`.
12. **Describe of a revision without `@`** takes no snapshot (no snapshot operation recorded).
13. **Expected commit**: `expected_commit_id` read before an unsnapshotted write refuses with EXPECTED_COMMIT_MISMATCH.

Existing: `guide_fits_its_budget`, the help golden test, `outputs_match_their_schemas`, the agent-files check in `just preflight`, MCP's `tools/list` bound.

## Performance and laptop cost

- `reef here` median wall time on this repository, idle workspace and after one write, before (`f72d0cd7`) and after, with `reef --version` as the floor. Gate: at most 12 ms idle on this machine.
- `reef describe -m` after one write, before and after: the fix adds the index sync (about 2 ms clean; a narrowed snapshot of the written paths otherwise).
- `just tokens scripted` for `state-own` before and after: the scripted arm's bytes must not grow. The live arm is a `tokens.yml` dispatch for the main session after merge.
- No disk, no new thread, no work on the snapshot path. `status/large/index_*` are untouched (no change to `GetStatus`).

## Acceptance criteria

- [ ] Each step of decision 4 has a test asserting `next` (tests 1, 7 to 9).
- [ ] A file written and described at once is in the described commit (test 11), through the CLI and MCP (one RPC).
- [ ] The guide within 1,800 bytes and its long part within 1,750; `reef --help` within 512 bytes; `AGENTS.md` regenerated.
- [ ] Brief `here` under 400 bytes for an idle workspace; `here` within 12 ms on this machine.
- [ ] `state-own`'s scripted bytes not higher.
- [ ] `just preflight`, `just doc`, the full suite.

## Verification

```sh
cargo nextest run -p reef-vcs --no-fail-fast -E 'test(here) | test(guide) | test(help) | test(outputs_match)'
cargo nextest run -p reef-daemon --test writes --no-fail-fast
# By hand, in a workspace of this repository:
reef ws run -- sh -c 'reef here; echo x > a.txt; reef here; reef describe -m "x"; reef here; reef show @ | grep a.txt'
```

## Out of scope

- Lifecycle fields in the daemon's API (decision 9), and the web UI.
- Evaluating the policy inside `here` (user's decision 3): `reef land` and `reef policy eval` answer it.
- `reef guide <topic>` pages (user's decision 5).
- Stacks in `here`: the step is the workspace's change's; `reef propose` and `reef land` take the stack below it themselves (R3.10).
- Brief views for the other commands #123 lists.

## Risks

- **The step lags a write the watcher has not reported** past `[status_index] sync_timeout`: the index then walks (`walk_reason` not synced), slower but right.
- **Track from `trunk()` alone**: a repository whose policy lives only on a non-trunk destination reads `publish` until its first proposal is recorded. Rare, and the first `reef propose` fixes it.
- **R3.45 merges first**: a proposed change in a repository with `[landing] automatic = true` lands without `reef land`. `next` stays `reef land`, which is harmless (it lands now or reports), and the follow-up below makes it `wait`.

## Choices this brief made, and why

- CLI-side composition, no API change (decisions 3 and 9): `here` is the CLI's command, the facts are already served, and R3.45 is changing the API in parallel.
- `step` separate from `hint`/`next`: health and lifecycle are two questions; tests and harnesses read `step`.
- `here` syncs the index: a step from a stale snapshot is wrong exactly when an agent asks.
- Concurrent reads: latency is the slowest read, not the sum.
- Describe syncs in the service, not under the rewrite's locks: the index sync is the cheap, narrowed snapshot reads and propose already use; an explicit snapshot under the locks would walk.
- Bookmark names in hints, never descriptions or reasons (S-216).
- No "your change landed" from a landed parent: a workspace created on a trunk head that someone else landed through Reef has the same parent, so the rule would lie to it. After a landing the workspace's new empty change reads `work`, and the agent's `op watch` has already said it landed.

## What later tickets inherit

- **R3.45**: when the destination's policy has `[landing] automatic = true`, step `land` renders `wait` (`next` the `op watch`). One match arm in `here.rs`; whichever of the two merges second adds it.
- **A daemon-side step** if MCP agents need it: `step()` is pure and moves as is.
- **#123's other views** follow `brief::Here`.

## Departures in the implementation

- **The step's read lives in `service/step.rs`**, not `crate::lifecycle::step`: it describes the commit through the service layer's `convert::change`, which is private to `service`. The pure `step()` and its table test are there too.
- **`reef here` makes two reads**, `GetWorkspace` with `sync_first` (or `snapshot_first` with `--snapshot`) and `ListPushes`, as before; the probes and `GetStatus` of decision 3 were for the CLI-only design.
- **MCP `get_workspace` always passes `sync_first`**, so its `step` covers what is on disk, as `here`'s does.
- **`reef --help` is unchanged.** `propose land conflict` do not fit its 512 bytes (511 used) by rewording alone, and the answers kept ADR 0051's budget.
- **`json::Change.subject` is in every change's full JSON**, not only in `status`, since a brief key must be a key of the full document (ADR 0045); only `status`'s trunk and parents use it in brief form.
- **The guide's Changes paragraph** no longer says that `reef publish`'s `next` opens the pull request (publish's own output says so); it names `reef here`'s `next`, `propose`, `land`, `publish`, `rebase` and `conflict show`. 1,864 bytes, long part 1,845.
- **Tests.** The lifecycle path end to end is `reef-daemon/tests/steps.rs` (no policy, a policy at trunk, a recorded proposal), and the CLI test covers `work`, `describe` and `publish` and the brief view. Conflicts, queued and landed are covered by the step table and `here`'s unit tests, not an integration test.
- **The web UI** shows "Next step" on a workspace's page.

## Departures from dev-phases.md

None: #262 and #327 are not `dev-phases.md` rows.

## Decisions for the user

**Answered 2026-11-18.** Questions 2, 3, 4 and 6 as recommended. **Question 1 changed**: the daemon computes the step and returns it with `GetWorkspace` (API revision 37), because many agents reach Reef only through MCP or an SDK. **Question 5 changed**: ADR 0025's guide budget rises by about 100 bytes, with the reason in the ADR, instead of trimming elsewhere; if the guide's growth makes `state-own` more expensive, the PR says so. **Added**: piped `reef status` gets a trimmed brief view (about 20 KB in an idle workspace here, mostly trunk's pull-request description repeated as the parent), measured before and after. "Answers applied" below replaces decisions 1, 3, 6 and 9 where they differ. The recommendations as they were put:

1. **Where the lifecycle is computed.** *Recommended*: in `reef here` only. *Alternative (chosen)*: the daemon serves it with `GetWorkspace` (API revision 37).
2. **A described change in a repository without proposals.** *Recommended (chosen)*: `next` stays `reef ws release`; the hint names `reef publish`.
3. **A proposed change.** *Recommended (chosen)*: `next` is `reef land`; no policy evaluation in `here`.
4. **A declined change.** *Recommended (chosen)*: "declined in round N", `next` `reef proposal show`; no reviewer text in `hint` or `next`.
5. **The guide.** *Recommended*: fit the current budgets by trimming. *Alternative (chosen)*: raise ADR 0025's budget by about 100 bytes.
6. **MCP parity.** *Recommended (chosen)*: one sentence in the MCP server's instructions.
7. **Numbers.** Now API revision 37, ADR 0080, S-216; schema 22 and S-217 to S-220 unused.

## Answers applied

These replace decisions 1, 3, 6 and 9 where they differ; the step table (decision 2) and the `hint`/`next` table (decision 4) stand.

**The API (revision 37, ADR 0080).** `lifecycle.proto` gains `enum ChangeStep` (`UNSPECIFIED`, `WORK`, `RESOLVE`, `DESCRIBE`, `PROPOSE`, `LAND`, `WAIT`, `REVISE`, `PUBLISH`, `DONE`). `Workspace` gains `step = 27` (`ChangeStep`), `lifecycle = 28` (the change's `ChangeLifecycle` at head) and `conflicted_path_count = 29`, and `GetWorkspace` also fills `changed_file_count` (20). All four are set by `GetWorkspace` for an active workspace and left unset elsewhere and at an operation, as `behind_trunk` is. `GetWorkspaceRequest` gains `sync_first = 6`: sync through the status index first (R2.23), as `GetStatus` does, so the step covers what is on disk; no snapshot when nothing changed since the last scan, else a snapshot of the changed paths, in `snapshot`. Ignored with `snapshot_first`; INVALID_ARGUMENT with `at_operation`.

**The computation, in the daemon.** `crate::lifecycle::step`: the facts (decision 2's `Change` and `Track`) read in one job on the repository's read thread at head, from the workspace's commit: the commit described, the changed-path count against its parents (the summary diff `GetStatus` uses), the conflicted-path count, `reef-policy.toml` in `trunk()`'s tree, and whether the repository has any `lifecycle` row; then the lifecycle filled as every read fills it; then the pure `step()`. Joined with `behind_trunk` and the overlaps in `get_workspace`, so it adds its own latency to theirs rather than after them. A failure leaves the four fields unset, logged, as the overlaps are.

**`reef here`** sends `GetWorkspace` with `sync_first` (with `--snapshot`, `snapshot_first` instead) and `ListPushes`, as two reads, and renders `hint` and `next` from `step`, `lifecycle` and the health fields. `cli.Inside` gains no keys of its own: the four fields come in the flattened `Workspace`. The brief view is decision 2's, with `step` and `conflicted_path_count` read from the workspace.

**Everyone else.** `reef ws inspect` prints a `step` line; MCP `get_workspace` passes `sync_first` (its result, `Inspected`, carries the workspace whole); the SDKs are regenerated; the web UI's workspace page shows the step beside the state. The MCP instructions sentence names `get_workspace`'s `step` and the finishing tools.

**The guide.** ADR 0025's numbers become 1,900 bytes for the guide and 1,850 for its long part (from 1,800 and 1,750), amended with the reason: the finishing path is what an agent set up by `reef init` most lacks, and `here` carries the per-state detail, so one paragraph is the whole cost. `reef --help` keeps its 512 bytes; `propose land conflict` go into its "Also" line only if they fit by rewording.

**`reef status`'s brief view.** Measured on this repository, an idle workspace's piped `reef status` is 19,902 bytes, most of it trunk's description and the same description again as the parent's. `json::Change` gains `subject` (the description's first line; empty for none), and the brief views of `RepositoryStatus.trunk` and `WorkspaceStatus.parents` keep `subject` and drop `description`; the working copy keeps its own `description`. ADR 0045's rules hold: `subject` is a key of the full document, and a brief key is never shortened. Before and after bytes go in the PR, and `state-own` (whose question is the base commit's subject) is rerun scripted.

**Tests added by the answers**: `lifecycle::step` unit tests, one per row of decision 2; a daemon test (`tests/workspaces.rs` or its own file) that `GetWorkspace` with `sync_first` reports `describe` right after a write with automatic snapshots off, `propose` after a describe, `land` after a propose with a policy at trunk, and `publish` without one; `api_v1`'s revision and digest; the SDKs' generated files; brief tests for `status`.
