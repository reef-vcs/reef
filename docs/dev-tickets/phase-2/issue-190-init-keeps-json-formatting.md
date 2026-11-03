# Issue #190 `reef init` keeps a JSON file's formatting

Phase 2 · Size S (no row; see "Size") · Fixes: issue #190 · Depends on: R2.13 (`.mcp.json`'s entry, ADR 0035 decision 13), R2.25 (`opencode.json`'s entry, ADR 0052 decision 2) · Unblocks: none listed (meant to land before the 0.4.0 tag)

## Context

`reef init` adds Reef's MCP server to a JSON file it shares with the user (`.mcp.json`, `opencode.json`) by parsing the whole file into an order-keeping value, inserting the entry, and printing the whole value again with `serde_json::to_string_pretty`. Keys and values survive. Everything else is reformatted: a tab-indented file comes out with two-space indents, one-line arrays and objects are spread over several lines, `1e5` becomes `100000.0`, `"é"` becomes `"é"`, and a file without a final newline gains one. On a large monorepo the diff touched every line of `.mcp.json`.

This ticket inserts Reef's member into the file's text instead, at the file's own indentation and line ending, and keeps every other byte. If the result does not parse to the same value today's code writes, the file is reprinted at its own indentation instead. It changes two functions in `crates/reef-vcs/src/agents/mod.rs` and adds about 130 lines there. There is no API, schema, CLI or output change, and a file with Reef's entry already in it is still never written.

### What was verified while writing this ticket

All of this was checked at d8ed637. Paths are under `crates/reef-vcs/` unless they start with `docs/`. A scratch prototype of the splice below (not committed) produced every expected output listed under "Tests", and each one parsed.

- **The files `reef init` merges into** (`Harness`, `src/agents/mod.rs:71-109`; paths at `:114-131`):

  | File | Harness | Reef's key | How it is merged today | Keeps formatting? |
  | --- | --- | --- | --- | --- |
  | `<root>/.mcp.json` | `McpJson` (repository only) | `mcpServers.reef` | `JsonTarget::MCP_JSON.merge` (`:950-978`) | **no** |
  | `<root>/opencode.json`, `$XDG_CONFIG_HOME/opencode/opencode.json` (`--user`) | `OpencodeJson { user }` | `mcp.reef` | `JsonTarget::OPENCODE.merge`, the same code | **no** |
  | `<root>/.codex/config.toml`, `$CODEX_HOME/config.toml` (`--user`) | `CodexConfig { user }` | `[mcp_servers.reef]` | `merge_codex_toml` (`:1046-1095`) | yes, see below |
  | `AGENTS.md`, `CLAUDE.md` | sections | between markers | `splice` (`:559-616`) | yes, byte for byte outside the markers |
  | skills, Cursor rule | whole files | the whole file | `whole_file` (`:796-813`) | Reef's own file |

  No other JSON file is written. Cursor gets only `.cursor/rules/reef.mdc`, no `.cursor/mcp.json`. Claude Code's user-level MCP file `~/.claude.json` is never edited (`:347-348`): the plugin is the user-level route. `.claude/settings.local.json` is only named in a note (`:357`). `duplicate_server_notes` (`:287-318`) reads `installed_plugins.json` and writes nothing.
- **The writer.** `JsonTarget::render` (`:919-943`) builds the value and ends `serde_json::to_string_pretty(&file)` plus `"\n"` (`:939-941`). `merge` (`:950-978`) calls it for the update case and passes the result through `keep_crlf` (`:981-987`), which turns every LF into CRLF when the file had any CRLF. The `Json` enum (`:876-890`, `#[serde(untagged)]` over `IndexMap`) already keeps key order, so order is not the bug. Its doc comment says outright that a merge changes "nothing but Reef's entry and whitespace (and a number's spelling)". R2.13's brief chose that ("only whitespace is normalized", `docs/dev-tickets/phase-2/R2.13-mcp-server.md:192`).
- **What `PrettyFormatter::with_indent` alone would give.** Checked with serde_json 1.0.154 (the lockfile's): a tab file parsed and reprinted with `with_indent(b"\t")` keeps the tabs but still spreads `"args": ["-y", "x"]` over four lines, writes `1e5` as `100000.0` and unescapes `é` and `\/`. Hand-written and formatter-written `.mcp.json` files keep short arrays on one line (Prettier and Biome both do; Biome indents with tabs by default), so indent detection alone would still touch most lines of a file like the one in the issue.
- **When the writer runs.** Only for `Action::Update`, when the file is a JSON object whose servers key is missing or is an object without `reef` (`:955-977`). A missing or blank file is `Action::Write` with `render(None)`. A `reef` entry equal to `render_mcp_entry` is `Action::Unchanged` with no contents, compared as values (`:960-964`), so its formatting is never looked at. A different `reef` entry is a skip ("`mcpServers.reef` is not reef init's"). A file that does not parse, is not an object, or has a non-object servers key is a skip ("not a JSON object").
- **How `--check` decides.** `cli/init.rs:277-321`: a file is stale only when its action is `Write` (missing) or `Update` (stale). `Unchanged` and every `Skip` pass. So formatting can only make `--check` flap if a written file fails to parse back to an `Unchanged` entry. The design below checks exactly that before it returns.
- **JSON with comments.** Every target is parsed with `serde_json`, which rejects comments and trailing commas, so such a file is skipped with "not a JSON object" and left alone. `opencode.jsonc` is detected by name and skipped with a reason that quotes the entry to paste (`:723-742`). opencode documents comments for `opencode.jsonc`; whether it also accepts them in `opencode.json`, and whether Claude Code accepts them in `.mcp.json`, was not checked (no opencode is installed here). It does not matter for this ticket: text reaches the new code only after `serde_json` has parsed it as strict JSON.
- **A byte order mark** also fails `serde_json::from_str` (checked), so a BOM file is skipped as "not a JSON object" today. Unchanged here.
- **Duplicate keys.** `serde_json` into `IndexMap` keeps the first position and the last value (`{"a":1,"b":2,"a":3}` reads as `{"a":3,"b":2}`; checked). So the servers object that counts is the last one of that name.
- **Codex's `config.toml` already keeps its formatting.** `merge_codex_toml` appends `[mcp_servers.reef]` after the file's text, so every existing byte is kept (`:1074-1079`; the test `codex_config_merge` asserts `merged.starts_with(theirs)`, `:1600`). When the file defines `mcp_servers` inline or by dotted keys, it falls back to `toml_edit`, which reprints only the table it changes. TOML has no indentation to detect. No change there.
- **`packages.rs:343-348`** (`pretty`) writes Reef's own plugin files (`plugins/claude-code/.mcp.json`, `hooks.json`, manifests), which are rendered whole and checked by `packages_are_current`. It is not a merge into a user's file and stays as it is. So does `render(None)`, the new-file form: this repository's committed `.mcp.json` is two-space and stays byte-identical.
- **Tests that pin today's layout.** `mcp_json_owns_only_reefs_entry` (`src/agents/mod.rs:1368-1426`) asserts the CRLF case reprints as `"\r\n  \"mcpServers\""` (`:1403`), which the new code no longer does. `opencode_json_merge` (`:1650-1708`) only checks key order. In `tests/init.rs`, `mcp_json_entry` (`:579-650`) checks CRLF and key order on a CRLF fixture (`:602`), and `init_writes_the_new_agents` (`:747`) parses `opencode.json`. `scripts/test-harness-packages.sh` and `reef-testing`'s Claude harness read only the plugin's files.
- **The tests run in the binary.** `agents` is a module of `reef`, not of the library: `cargo nextest run -p reef-vcs --bin reef agents::`.

## Decisions

### Size

S, with no `dev-phases.md` row. Two functions in one file change: about 130 lines of new code (a scanner over already-parsed JSON, two layout helpers, the splice, the check and the fallback), one table test with fifteen cases, two existing unit tests and one integration test adjusted, one integration case added, and three documentation edits. The indent-only fix the issue suggests is about 30 lines, but it leaves the reported diff mostly in place (see above). Question 1 asks the user to confirm the splice.

### The rule

Reef inserts one member into the file's text and changes no other byte. The member is `"reef": <entry>` in the servers object, or `"<servers>": {"reef": <entry>}` at the end of the top-level object when there is no servers key. It goes after the last member of that object, as `IndexMap::insert` puts it today, so the parsed result is the same value today's code writes. Everything outside the inserted bytes is kept: indentation, line endings, one-line arrays, number and string spellings, the final newline or its absence, whitespace before the first `{`.

### Where the layout comes from

Let `C` be the object the member goes into: the servers object (the last top-level member with the servers key, since that is the one `serde_json` reads), or the top-level object. Let `p` be the whitespace before `C`'s key on its line (for the top level, before the `{`), or none when something other than whitespace comes before it on that line.

- **Line ending:** `"\r\n"` if the file has any CRLF, else `"\n"`. Only the inserted bytes use it. `keep_crlf` is not applied to a splice, so a file with mixed endings keeps them.
- **`C` has members.** Let `L` be the last one.
  - **`L`'s key starts its line** after whitespace `m`: the member goes on its own lines. After `L`'s value, insert `,` + eol + `m` + `"key": ` + the value printed by `PrettyFormatter::with_indent(unit)`, with `m` after each of its line breaks. The unit is `m` without the prefix `p` when `p` is a proper prefix of `m`; otherwise the file's unit (below).
  - **Otherwise** (a one-line object, or several members on `L`'s line): insert `, "key": ` + the value inline, as `{"type": "stdio", "command": "reef", "args": ["mcp"]}`: `", "` between items and `": "` after keys, which is how a person writes one line. A one-line file stays one line; its diff is that one line either way.
- **`C` is empty** (`{`, whitespace, `}`). If `p` is some (always true for a top-level `{` at the start of a line): replace the bytes between the braces with eol + `p`+unit + the member printed as above with `p`+unit after each break + eol + `p`. Otherwise the member goes inline between the braces.
- **The file's unit,** used when there is no sibling to copy: the leading whitespace of the first line after the first that has non-whitespace after its indentation: `"\t"` if it starts with a tab, else its run of spaces. Two spaces when no line is indented. JSON strings hold no raw newline, so every line start is outside a string. This is the issue's "first indented line" rule, used only as a fallback because a sibling's own indentation is right even in a file with mixed indentation.
- **Reef's own entry,** when on its own lines, is in `serde_json`'s pretty layout at that unit, with `"args"` over three lines, as a new file has it today. Matching the file's habit for short arrays is not attempted: those are Reef's lines, and a formatter that rewrites them touches only them.

The issue's file before and after (tabs shown as tabs):

```
{
	"mcpServers": {
		"other": {"command": "npx", "args": ["-y", "x"]}
	},
	"n": 1e5
}
```

```
{
	"mcpServers": {
		"other": {"command": "npx", "args": ["-y", "x"]},
		"reef": {
			"type": "stdio",
			"command": "reef",
			"args": [
				"mcp"
			]
		}
	},
	"n": 1e5
}
```

The diff is one changed line (`,` after `other`'s value) and eight added lines. Today it is every line.

### Finding the bytes

The text has already parsed as a JSON object (`merge`, `:955`), so the scanner needs no error handling of its own. It walks bytes: whitespace is ` \t\n\r`; a string runs to the next `"` not escaped by `\`; an object or array runs to its matching close, skipping strings; a scalar runs to the next `,`, `}`, `]` or whitespace. `members(text, open) -> (Vec<Member>, close)` lists an object's members, each with its key unescaped by `serde_json::from_str::<String>` (so `"mcpServers"` matches), the byte offset where the key starts, and its value's byte range. Every structural byte is ASCII, so each offset is a `char` boundary. A scanner that meets anything it does not expect returns `None`, and the fallback runs.

### The check, and the fallback

`merge` keeps its current tests and order. For the update case it becomes:

1. Build `expected`: the parsed file with Reef's entry inserted, as `render` does today. Split `render` into `with_entry(self, file) -> IndexMap<String, Json>` and the printing, so `render(None)` and `with_entry` share one insertion.
2. `splice(self, text) -> Option<String>` as above.
3. Use the splice only if `serde_json::from_str::<Json>(&spliced) == Ok(Json::Object(expected))`. `Json` already derives `PartialEq`, and a number parsed from the same text compares equal. This is the guarantee `--check` needs: the written file parses to a `reef` entry equal to `render_mcp_entry`, so the next `reef init` and `--check` see `Unchanged`.
4. Otherwise, `reformat(text, expected)`: `expected` printed with `PrettyFormatter::with_indent(file's unit)`, `keep_crlf`, and a final line ending only if `text` ended with one. This is the issue's first suggestion. No known input reaches it; it is there so a scanner bug costs layout, never content.

Duplicate servers keys pass the check: the splice goes into the last one, and parsing keeps the first position with the last value, which is what `expected` has (case 13 below).

### What does not change

- **A file with Reef's entry** is `Unchanged` and never written, whatever its formatting. That is today's behavior; test cases assert it for each layout.
- **A new or blank file** (`render(None)`): two-space, final newline, `$schema` first for opencode. Byte-identical.
- **Skips:** a different `reef` entry, a file that is not a JSON object (including JSONC and a BOM), a non-object servers key, `opencode.jsonc`. Same reasons.
- **`config.toml`, the Markdown sections, whole files, the plugin trees.**
- **`--check`, `reef init`'s JSON and text, the notes.** An updated file is still reported `updated`.

### ADR 0035

Decision 13 says "The file is merged in its own order". Amend it in place to: "The file is merged in its own order and layout: Reef's member is inserted into the text at the file's indentation and line ending, and no other byte changes (#190)." Add `#190 (decision 13)` to its "Amended by" line. ADR 0052 decision 2 says `opencode.json` is merged "as `.mcp.json` is merged", so it needs no edit. No new ADR: this narrows how an existing decision is carried out.

## Deliverables

- **`crates/reef-vcs/src/agents/mod.rs`:** the scanner, `splice`, `with_entry`, `reformat` and the new `merge` above, all on or beside `JsonTarget`. Update the doc comments on `Json` (`:876-880`: a merge changes only Reef's member, as text), `render` (`:919-921`: the new-file form only), and `merge` (`:944-949`: name the rule and the fallback).
- **Tests:** below.
- **`docs/adr/0035-mcp-server.md`:** the amendment above.
- **`CHANGELOG.md`**, Unreleased, **Fixed**: "`reef init` adds Reef's MCP server to an existing `.mcp.json` or `opencode.json` without reformatting it: the rest of the file keeps its indentation (tabs or spaces), line endings, one-line arrays, number and string spellings and final newline, and only Reef's lines are added ([#190](https://github.com/reef-vcs/reef/issues/190))."
- **`docs/dev-tickets/phase-2/README.md`:** the row `| #190 | [issue-190-init-keeps-json-formatting.md](issue-190-init-keeps-json-formatting.md) | S | R2.13, R2.25 | none listed |`.
- **Unchanged:** `packages.rs`, `cli/init.rs`, the proto, `API_REVISION` (14), the guide, `docs/quickstart.md`, this repository's `.mcp.json` and plugin trees.

## Tests

### `crates/reef-vcs/src/agents/mod.rs`, in the existing `mod tests`

1. **`json_merge_keeps_the_files_layout`**, a table of `(target, before, after)` with `after` exact. Write the strings with `\t`, `\n` and `\r\n` escapes. `E` below is Reef's `.mcp.json` entry printed on its own lines at the case's unit (`"type": "stdio",` / `"command": "reef",` / `"args": [` / `"mcp"` / `]`), `I` is it inline: `{"type": "stdio", "command": "reef", "args": ["mcp"]}`. For every case also assert: `merge(Some(before)) == (Action::Update, Some(after))`; `merge(Some(after)) == (Action::Unchanged, None)` (no flap); and `after` parses to the same `Json` as `render(Some(file))` does today.
   1. **The issue's file:** tabs, `other` with an inline `args`, `"n": 1e5`, final newline. `after` is the example under "Where the layout comes from", byte for byte; `1e5` and the inline array are kept.
   2. **Four spaces, no final newline,** `other` spread over lines: Reef's member at eight spaces, nested lines at twelve and sixteen; the file still ends in `}` with no newline.
   3. **CRLF**, two spaces: the `tests/init.rs:602` fixture. Every inserted line break is `\r\n`; `after.matches('\n').count() == after.matches("\r\n").count()`.
   4. **Mixed line endings:** LF lines with one CRLF line. Inserted breaks are `\r\n`, and every LF-only line before is still LF-only.
   5. **One line:** `{"z": 1.5, "mcpServers": {"other": {"command": "x", "b": true, "a": null}}, "a": [2, 1]}` becomes the same with `, "reef": I` after `other`'s value, and nothing else changed.
   6. **`{}`** becomes `{\n  "mcpServers": {\n    "reef": E\n  }\n}` with no final newline; **`{}\n`** becomes exactly what `render(None)` writes for `.mcp.json` today; **`{ }\n`** the same as `{}\n`.
   7. **An empty servers object,** tabs: `{\n\t"mcpServers": {}\n}\n` becomes `{\n\t"mcpServers": {\n\t\t"reef": E\n\t}\n}\n`.
   8. **No servers key,** tabs: `{\n\t"other": 1\n}\n` becomes `{\n\t"other": 1,\n\t"mcpServers": {\n\t\t"reef": E\n\t}\n}\n`.
   9. **Escapes:** `{"s": "café \/ \"}", "mcpServers": {}}`: the string is kept as written (including the `}` inside it), and `"reef": I` goes inline between the braces.
   10. **Leading whitespace:** `\n\n{\n  "mcpServers": {...}\n}\n` keeps the two blank lines.
   11. **Mixed indentation:** top level at two spaces, servers at a tab: Reef's member copies the last server's own indentation.
   12. **Several servers on one line inside a multi-line object:** `"mcpServers": {\n    "a": {}, "b": {}\n  }`: `, "reef": I` goes after `b`'s value, on that line.
   13. **Duplicate servers keys:** `{"mcpServers": {"a": {}}, "mcpServers": {"b": {}}}`: Reef's member goes into the second, and the check passes.
   14. **opencode,** tabs: `{\n\t"model": "x",\n\t"mcp": {\n\t\t"other": {"type": "remote"}\n\t}\n}\n` gains `"reef"` with opencode's entry (`"type": "local"`, `"command": [`…`]`, `"enabled": true`) at two tabs, and no `$schema`.
   15. **Unchanged entries rewrite nothing:** a tab-indented file and a one-line file that already have Reef's entry, inline or spread, give `(Action::Unchanged, None)`.
2. **`json_reformat_keeps_unit_endings_and_final_newline`**: `reformat` on a tab file with CRLF and no final newline prints tabs, CRLF and no final newline; on a four-space LF file with one, four spaces, LF and one. This is the fallback; no input to `merge` is known to reach it.
3. **`mcp_json_owns_only_reefs_entry`**: replace the CRLF assertion at `:1403` with: the output is the input (whose two `, "a"` became `,\r\n"a"`) with `, "reef": I` inserted after `other`'s value, and it has exactly the input's two `\r\n`. The rest of the test is unchanged and passes.
4. **`opencode_json_merge`**, **`codex_config_merge`**: unchanged, and pass.

### `crates/reef-vcs/tests/init.rs`

5. **`mcp_json_entry`**: after the CRLF fixture at `:602`, assert `merged` byte for byte (case 3's `after`). Then add the issue's case through the CLI: write case 1's `before` with a second server; `reef init --no-register --agents mcp` reports `.mcp.json` `updated` and the file equals case 1's `after`; a second run reports `unchanged` and the file's bytes are the same; `reef init --check --agents mcp` exits 0.
6. **`init_writes_the_new_agents`** (`:747`): unchanged. Its `opencode.json` (`{"model": "x"}\n`) now becomes one line with `"mcp": {"reef": I}` inline, which its parse-based assertions accept.

Every other test passes unchanged, including `packages_are_current`, `just agent-files` and `scripts/test-harness-packages.sh`.

## Implementation notes

- **Order:** the scanner and `splice` with test 1; then `with_entry`, `reformat` and the new `merge` with tests 2 to 4; then test 5; then the ADR, CHANGELOG and README.
- **Keep it in `agents/mod.rs`,** next to `JsonTarget`. It is private to the merge, like `codex_entry`. No new dependency: a format-keeping JSONC crate (`jsonc-parser` with its `cst` feature) would also read comments, but adds a dependency and supply-chain review before a tag, and comments are out of scope.
- **No `unwrap` on the scanner path.** It returns `Option`, and `None` falls through to `reformat`. `clippy::unwrap_used` is a warning outside tests, and `just ci` runs clippy with `-D warnings`.
- **`render_mcp_entry`'s value is unchanged,** so `mcp_shapes_agree` and the packages do not move.
- **No `cfg(target_os = "linux")` code** is touched, and no timing test is involved.

## Acceptance criteria

- [ ] `reef init` adding Reef's entry to an existing `.mcp.json` or `opencode.json` (repository or `--user`) changes only the bytes of Reef's member and the one `,` before it: indentation, line endings, inline arrays, number and string spellings, leading whitespace and the final newline or its absence are kept (tests 1, 5).
- [ ] A file with Reef's entry is never written, and `reef init --check` passes on any file `reef init` wrote (tests 1.15, 5).
- [ ] A new or blank file, every skip, and `config.toml` are byte-identical to before (tests 1.6, 3, 4, 6).
- [ ] No API, schema or output change. `api_digest_matches_revision` passes at revision 14.
- [ ] ADR 0035, CHANGELOG and the tickets README are updated. `just ci` passes.

## Verification

```sh
cargo nextest run -p reef-vcs --bin reef agents::
cargo nextest run -p reef-vcs --test init
just agent-files
just ci
```

By hand, from a scratch directory: write case 1's `before` as `.mcp.json`, run `reef init --no-register --agents mcp`, and check that `git diff --no-index` against a copy shows one changed line and eight added lines.

## Out of scope

- **JSON with comments or trailing commas** in `.mcp.json` or `opencode.json`: skipped as "not a JSON object", as now. Its reason could say what `opencode.jsonc`'s does ("comments are not rewritten; add … by hand"); see question 3.
- **A byte order mark:** skipped as "not a JSON object", as now.
- **Mixed line endings in `config.toml`**: `keep_crlf` still makes a mixed file all CRLF there. Rare, and Codex's files are TOML written by people or by Codex.
- **Reef's own entry matching the file's array style** (`"args": ["mcp"]` on one line in a file that writes arrays that way).
- **A new file's layout** (`render(None)`) and the plugin trees' (`packages.rs`).

## Departures, and questions for the user

This departs from R2.13's brief (`R2.13-mcp-server.md:192`: "only whitespace is normalized") and narrows ADR 0035 decision 13, amended above. No other ADR or `dev-phases.md` row is affected.

Decided by the user on 2026-11-03:

1. **The splice**, with indent-aware reprinting as its fallback, as above.
2. **A one-line object gets Reef's member inline**, as above.
3. **The JSONC reason is in this ticket.** A `.mcp.json` or `opencode.json` (repository or `--user`) that fails to parse as strict JSON but looks like JSON with comments or trailing commas is skipped with a reason like `opencode.jsonc`'s (`src/agents/mod.rs:723-742`): comments are not rewritten, and the reason quotes the entry to add by hand. A file that is not JSON at all keeps "not a JSON object". The detection is a heuristic (a `//` or `/*` outside strings, or a `,` before `}` or `]`), so prefer a false "not a JSON object" to a false JSONC reason. Add a unit case for each: comments, a trailing comma, and a file that is neither. Add a CHANGELOG line, under the same **Fixed** entry or its own. The "Out of scope" bullet on JSONC now covers only *writing into* such files.
