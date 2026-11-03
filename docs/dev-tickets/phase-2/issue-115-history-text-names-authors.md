# Issue #115 History text names authors

Phase 2 · Size S (no row; see "Size") · Fixes: issue #115 · Depends on: R2.16 (`reef history`, its text form, the history index), R2.17 (the `who-file` and `who-symbol` questions and their sufficiency check) · Unblocks: none listed (a `reef (text)` number for `who-file` and `who-symbol` in the token benchmark)

## Context

`reef history -o text` prints one line per change with a "who" column built from the change's activity rows. A row from an operation with no agent (a git import, a fetch, or another jj client's write; R2.16 and ADR 0044 decision 2) prints as `-`. When a change has only such rows, the column says `-` and nothing else, although the index keeps the commit author's email beside every change and the JSON prints it. So an agent reading text cannot say who wrote landed history. In R2.17's benchmark both `history ... -o text` candidates miss `bot-2`, the landed author, and the `reef (text)` cells for `who-file` and `who-symbol` are `–` in every results row.

This ticket prints the author's email, in angle brackets, when a change has no agent. It also drops the `-` that stands beside named agents. It is a change to one text renderer, with no API, schema or JSON change.

### What was verified while writing this ticket

- **Today's output**, from the root checkout at 9c05c43 (`reef history -o text --limit 0 crates/reef-client/src/json.rs`): 33 changes, 4,386 bytes before the tail line. Every line has an agent-less activity row. 18 lines print `-` alone (all the landed squash merges), and 15 print `-, claude@<host>` (the agents' own changes, later abandoned). `-o json` has `"author_email":"arsood@gmail.com"` on all 33, with an `activity` row whose `agent` is `null`.
- **The renderer.** `HistoryText` in `crates/reef-vcs/src/cli/render_vcs.rs:858-975`. The who column is built at `:896-915`: distinct `activity[].agent`, with `None` as `"-"`, the first three joined by `", "`, then ` +N`, or `-` when empty. The line is written at `:925-934` as `"{id}  {outcome:<10} {who:<14} {last_at}  {subject}"`. Anything over 14 characters already pushes the time one space right, as `claude@Aarons-MacBook-Pro` does. `cli/history.rs:205` prints it through `ctx.out.object`. The renderer has no unit tests. The test module above it (`:747-856`) has a `render(&dyn TextRender) -> String` helper.
- **The data.** `HistoryChange.author_email` is `Option<String>` (`crates/reef-client/src/json.rs:3009-3056`, field at `:3026`), from proto field 7 (`crates/reef-api/proto/reef/api/v1/history.proto:174`, "the email of its latest commit's author"). It is stored in `changes.author_email TEXT NOT NULL` (`crates/reef-daemon/src/history/store.rs:30`) and read from `commit.author().email` (`crates/reef-lib/src/jj/history.rs:31`, `:74`). **The author's name is stored nowhere in the history index.** `ChangeActivity.agent` is "`null` for another jj client's writes and imports" (`json.rs:2947-2960`).
- **Imports and retention.** An `IMPORT_REFS` operation that creates a change dates it by commit time and skips commits older than `[history] retention`, 90 days by default (`crates/reef-daemon/src/history/follow.rs:296-309`, `config.rs:622-630`). `reef_testing::git::commit` dates every commit 2023-11-14 (`crates/reef-testing/src/git.rs:51`), so a test that imports through it must raise the retention.
- **The benchmark.** `who-file` and `who-symbol` are in `crates/reef-testing/tokens/scenario.toml:332-402`. The `history text` and `history by name[ and path], text` candidates carry no `insufficient` mark. They fail `missing_names` (`crates/reef-testing/src/tokens/scripted.rs:186-195`, applied at `:415`) because their output lacks `bot-2`. The fixture authors landed commits `bot-N <bot-N@agents.invalid>` and the import `import <import@fixture.invalid>` (R2.17, "The import" and "The landed sessions"). `tests/tokens.rs:250-289` already checks that the JSON reports the landed change by `author_email`. `reef (text)` is reported, never gated (`docs/token-benchmark.md:64`).
- **Other surfaces.**
  - Piped `reef history` (brief) prints the full `History` compactly, since ADR 0045 gave it no view, and it has `author_email`.
  - MCP `query_changes` returns the same `History` (`crates/reef-vcs/src/mcp/results.rs:48-53`, `tools.rs:891`), with `author_email`.
  - `list_changes` and `get_change` return `Change` with `author {name, email, at}`.
  - `reef show -o text` prints `author: Name <email> <time>`.
  - `reef log -o text` prints no author for any change, by design (R2.27).
  - The web UI's change page shows the author's signature from `GetChange` (`web/src/views/ChangeDetail.tsx:427`).
  - `reef history --group-by agent` prints `-  33` for agent-less changes, in text and JSON alike.

## Decisions

### Size

S, with no `dev-phases.md` row: about fifteen lines in one renderer, four unit tests, one integration test, one benchmark guard, and three documentation lines.

### The who column

For each change, in this order:

1. **Named agents**: the distinct non-null `activity[].agent` values in activity order (newest first), the first three joined by `", "`, then ` +N` for the rest. Agent-less rows are skipped and no longer print as `-`.
2. **No named agent**: `<` + `author_email` + `>`.
3. **Neither**: `-`.

The column stays padded to 14 characters. The id, outcome, time and subject columns, the touch and path lines, the `--group-by` lines and the tail line are unchanged.

Before and after, from this repository's real output:

```
rzptpnxqukmz  landed     -              2026-11-01T17:38:29.923Z  R2.24: reef schema (#148)
    crates/reef-client/src/json.rs
mkyylktxprus  abandoned  -, claude@Aarons-MacBook-Pro 2026-11-01T17:38:29.923Z  R2.24: reef schema
    crates/reef-client/src/json.rs
```

```
rzptpnxqukmz  landed     <arsood@gmail.com> 2026-11-01T17:38:29.923Z  R2.24: reef schema (#148)
    crates/reef-client/src/json.rs
mkyylktxprus  abandoned  claude@Aarons-MacBook-Pro 2026-11-01T17:38:29.923Z  R2.24: reef schema
    crates/reef-client/src/json.rs
```

On the fixture the `who-file` line becomes `… landed     <bot-2@agents.invalid> …`, which names `bot-2`.

**Cost.** Over the 33 changes above (simulated from the JSON), the output goes from 4,386 to 4,422 bytes, +0.8%. The 18 emails run 2 bytes past the 14-character pad (+36) and their brackets add 36. Dropping `-, ` saves 36, because on the three `claude@Mac` lines it sat inside the pad.

**Why the author only when there is no agent.** For changes Reef wrote, the agent answers "who", and the author is the user's configured identity, which is the same on every line (19 more bytes a line here). `-o json` keeps both. **Why brackets.** Agent ids look like emails (`claude@host`), and `--by` takes agents, not authors. `<…>` is git's and `reef show`'s notation for an email, so a reader knows this is an author.

### Email, not name: no API change

Only the email is in the index, so the email is what text prints. A name would need a new `changes` column, a backfill of existing rows, a new `HistoryChange` field (API revision 15) and an ADR 0044 amendment, all for output this ticket wants smaller. The email is also what `git log --format=%ae` and the JSON give. API revision stays 14 and the database schema stays 6. No ADR: ADR 0044 and R2.16 leave the text wording to the implementer, and ADR 0045 decision 5 makes the full JSON the interface for programs. Text has no contract.

### The other surfaces

Only `reef history -o text` has this bug. Brief and full JSON, MCP `query_changes`, `list_changes`, `get_change`, `reef show` and the web UI all give the author. The rest is listed under "Out of scope".

### The benchmark

No scenario change, so no new baseline: the scenario's digest is what starts one, and the candidates' sufficiency is computed, not declared. After this ticket the three text candidates (one for `who-file`, two for `who-symbol`) name `bot-2` and become sufficient. The `reef (text)` cells for `who-file` and `who-symbol` get numbers, and no gated cell moves. A guard keeps it that way (test 6).

## Deliverables

- **`crates/reef-vcs/src/cli/render_vcs.rs`**: the who column above, and the doc comment on `HistoryText` naming the rule ("its agents, or its author's email in angle brackets when no agent wrote it").
- **`crates/reef-vcs/src/cli/render_vcs.rs` tests** and **`crates/reef-vcs/tests/vcs.rs`**: tests 1 to 5.
- **`crates/reef-testing/tests/tokens.rs`**: test 6.
- **`CHANGELOG.md`**, Unreleased, **Fixed**: "`reef history -o text` names a change's author, as `<email>`, when no agent wrote it (a change imported from git or written by another jj client), and no longer lists `-` beside the agents of a change that has some (#115)."
- **`docs/token-benchmark.md:76`**: after the #115 link, "fixed by #NNN" (this PR).
- **`docs/dev-tickets/phase-2/README.md`**: the row `| #115 | [issue-115-history-text-names-authors.md](issue-115-history-text-names-authors.md) | S | R2.16, R2.17 | none listed |`.
- **Unchanged**: the proto, `API_REVISION`, ADRs, `reef history --help`, the guide, `scenario.toml`, R2.16's and R2.17's briefs.

## Tests

### `crates/reef-vcs/src/cli/render_vcs.rs`, in the existing `mod tests`

Add a helper `history_change(agents: &[Option<&str>], author: Option<&str>) -> HistoryChange` with fixed id, outcome `landed`, `last_at` and subject, and one matched path. `HistoryChange` has no `Default`.

1. **`history_text_names_the_author_without_an_agent`**: with activity `[None]` and author `bot-2@agents.invalid`, the output is exactly `"<12-char id>  landed     <bot-2@agents.invalid> <last_at>  <subject>\n    <path>\n1 change\n"`. With no activity rows at all, the line is the same.
2. **`history_text_lists_agents_not_the_author`**: with activity `[Some("bot-4"), None, Some("bot-5"), Some("bot-4")]` and an author set, the who column is `bot-4, bot-5` padded to 14. The line has no `-,` and no `<`.
3. **`history_text_caps_agents_and_skips_agentless_rows`**: five distinct agents and two `None` rows give `a, b, c +2`.
4. **`history_text_dash_without_agent_or_author`**: activity `[None]` and author `None` give `-` padded to 14, so the time starts at the same column as today.

### `crates/reef-vcs/tests/vcs.rs`

5. **`history_text_names_authors`**, a new test beside `history` (which is already over the line limit):
   - **Setup.** A `TestDaemon::with_synthetic_repo_and(Preset::Small, …)` that sets the watcher as `fixture()` does and raises `config.file.history.retention` to five years, then sets the history throttle to zero as `history` does. Use a plain git repository under `daemon.tmp` made with `reef_testing::git::init_repo`, as `vcs.rs:228-245` does.
   - **The agent-less change.** Run `reef status` there, which registers the repository. Then `reef_testing::git::commit(&plain, "refs/heads/main", Some(&first), &[("README", …), ("landed.txt", "landed\n")])`, and `reef ws create` from `plain`, whose import picks the commit up. That create's workspace is the agent's.
   - **The agent's change.** Write `agent.txt` in that workspace and run `reef --agent bot-1 status -o json` there.
   - **Poll** `reef history -o json` until both changes are listed with `touches_state` `current`, as `history` does.
   - **Assert:**
     - in `-o json`, the `landed.txt` change has `author_email` `reef-test@example.com` and only `null` agents;
     - `reef history landed.txt -o text` has a line with `landed` and `<reef-test@example.com>`, and no line with `  -  `;
     - `reef history agent.txt -o text` has a line that names `bot-1`, with no `<` and no `-,`.

### `crates/reef-testing/tests/tokens.rs` (ignored; `just tokens-check`)

6. **In `fixture_and_truth`, beside the "every arm has a sufficient candidate" loop (`:344-352`)**: for `who-file` and `who-symbol`, `cheapest(&results, id, Arm::Reef, "bytes", |c| c.text_output)` is `Some`. This is #115's regression guard.

Every other test passes unchanged. The only text assertion on `reef history` today (`vcs.rs`, `history`: `+ function parse()  hist/parser.py` and the `1 change` tail) does not read the who column.

## Implementation notes

- **Order:** the renderer with tests 1 to 4; then test 5; then test 6 with `just tokens-check`.
- **Check the benchmark locally.** It is a few minutes, and free:

  ```sh
  just tokens scripted question=who-file,who-symbol --no-sweep --source /Users/arsood/Projects/ReefVCS/reef
  ```

  Run it from the ticket workspace, which has no git. Read `crates/reef-testing/tokens/results/latest.md`: the `reef (text)` cells for both questions should have numbers. Put them in the PR. If the fixture's history includes the import commit, its line shows `<import@fixture.invalid>`. That is right, and it does not affect sufficiency.
- **Leave `activity` in JSON alone**: the `null` agent rows stay in brief, full and MCP output.
- **Timing tests flake under load** on `main`. Test 5 polls with a deadline, as `history` does, and never waits on the watcher, since it snapshots with `reef status`.
- **No `cfg(target_os = "linux")` code** is touched.

## Acceptance criteria

- [ ] `reef history -o text` prints `<email>` in the who column for a change with no named agent, named agents only otherwise, and `-` when there is neither (tests 1 to 5).
- [ ] No API, schema or JSON change. `api_digest_matches_revision` passes at revision 14.
- [ ] `just tokens-check` passes with test 6. The PR gives the local `reef (text)` numbers for `who-file` and `who-symbol`.
- [ ] CHANGELOG, `docs/token-benchmark.md` and the tickets README are updated. `just ci` passes.
- [ ] After merge, `tokens.yml` is dispatched with `trials: 0` and `questions: who-file,who-symbol` (scripted only; nothing gated moves) and the notes "#115: history text names authors", and its results PR is merged.

## Verification

```sh
cargo nextest run -p reef-vcs --lib render_vcs
cargo nextest run -p reef-vcs --test vcs history
just tokens-check
just tokens scripted question=who-file,who-symbol --no-sweep --source /Users/arsood/Projects/ReefVCS/reef
just ci
```

## Out of scope

- **`--group-by agent`** puts every agent-less change in one `-` group, in text and JSON. Grouping or filtering by author (`--group-by author`, `--author`) is an API change (revision 15). The PR files one follow-up issue for both.
- **The author's name** in history. It needs a column, a backfill and API revision 15; see above.
- **`reef log -o text`** prints no author for any change. That is R2.27's format, not this bug, and `reef show` has the author.
- **The web UI's activity list** renders an agent-less row as `- in …`. It is cosmetic, and the page shows the author above it.
- **A brief view of `reef history`**: ADR 0045's follow-up, unchanged here.

## Departures and decisions

No departure from `dev-phases.md` (no row) or any ADR. The user decided two choices on 2026-11-03:

1. **Brackets around the email: yes.** `<bot-2@agents.invalid>` costs 2 bytes a line, marks it as an author, and matches git and `reef show`. A bare email reads like an agent id such as `claude@host`, which `--by` would not find. On the 33-change example, brackets give 4,422 bytes; a bare email would give 4,386.
2. **Drop the `-` beside named agents: yes.** Today 15 of the 33 lines read `-, claude@<host>`. That `-` is the fetch or import that rewrote or abandoned the change, not a writer. `-o json` keeps the row.
