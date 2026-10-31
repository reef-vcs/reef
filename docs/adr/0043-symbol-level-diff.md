# 0043. Symbol-level diff

- Status: accepted
- Date: 2026-10-30
- Ticket: R2.15
- Amended by: [0044](0044-history-index-and-query-layer.md)

## Context

The [design](../design.md) says diffs "are available at the symbol level as well as the line level, which is both more useful to an agent and far cheaper in tokens than a unified diff" (Query layer). It also says the review surface defaults to "a symbol-level summary with affected call sites" that expands to lines (Interaction model). Phase 2's exit criterion asks for symbol-level diffs in Python, TypeScript, Go and Rust. [0042](0042-symbol-index.md) built the index this reads. Each entry is keyed by path and file content, and each definition carries its range, its enclosing definition and a digest of its bytes. R3.18 will choose checks from the answer, so it must say where the index could not answer.

## Decision

1. **Fields on `GetDiff`, not a new RPC.** `symbols`, `max_call_sites`, `expand_symbols` and `symbol_source` go on the request. `FileDiff.symbols` and `GetDiffResponse.symbol_summary` go on the response, at the numbers R2.02 left free (API revision 8). A symbol diff views the same diff: the same two sides, paths, rename detection and pinned pages. Files with no symbol answer stay in the list with their coverage. `symbols` is a flag beside `format`, not a fourth `DiffFormat`, because `SUMMARY` and `STAT` combine with it, and `HUNKS` with `expand_symbols` is "expand to lines".
2. **One source per file pair.** A pair answers from SCIP when every side that exists has an upload for its exact content, and from tags otherwise. `symbol_source` can force one. The two are never mixed within a pair: a SCIP side against a tags side would show every symbol rewritten.
3. **Matching across the whole diff by symbol string.** Definitions are grouped by symbol string over every differing file, not per pair. So a Go method moved between files of one package, or a SCIP symbol whose identity is the package's, pairs across files. On the old side of a renamed file, the path's components in a definition's symbol are first rewritten to the new path's. For tags this means the leading namespaces (the directory's, for Go). For SCIP it means a run of namespaces equal to the old path, less a common prefix, which covers an indexer run in a subdirectory. Copies are not rewritten, because a copy's definitions are new. Symbol strings are compared after parsing and formatting, so two indexers' escaping cannot split one symbol.
4. **Pairing by digest, then order.** Within one symbol, old and new definitions pair by equal digest first, then in source order, which handles overloads and conditional definitions. A pair whose digests differ is `CHANGED`. A pair with equal digests in different files is `MOVED`, except for the file's own rename. Unpaired definitions are `REMOVED` and `ADDED`, except that one removed and one added definition with the same name, kind and digest become `MOVED`, with `old_symbol` set. That catches moves under identities that spell the file. A definition with no digest is `CHANGED` only when a changed line of its file touches its name.
5. **Nesting from line diffs.** A parent whose bytes changed only inside its nested definitions is reported with `nested_only`. That holds when every changed line in its range, on both sides, lies in a direct child's range. Changed lines that are only whitespace do not count. Without that, removing a method would always take the blank line before it and clear the flag. Per-symbol line counts come from the same diff of the two definitions' lines.
6. **Coverage per file, and the 1,000-file limit.** Every file of a symbol diff says `COVERED`, `UNSUPPORTED`, `SKIPPED` (with the side's skip reason), `NO_SCIP` or `DIFF_TOO_LARGE`. Symbols are computed only when at most 1,000 files differ. That is R2.02's rename limit, and matching across renamed files needs the renames. The diff never fails for a missing index. A symbol pass that fails is logged, and its page's files are `SKIPPED` with no reason. Only a disabled index fails the request, with `SYMBOLS_DISABLED`.
7. **Entries made inline; the new tree indexed for call sites.** Each differing file's entry on each side is looked up, and missing tags entries are extracted on the spot. Call sites need the new tree complete. The request waits up to 10 s for its pass, holding no read permit, and past that marks the changes `INDEX_INCOMPLETE`. Extraction and the store's reads run under the read permit, as `ListSymbols`' inline extraction does. The brief asked to take them outside it, but they are bounded by the 1,000-file limit (a cold 100-file diff on `large` takes 0.6 s), and splitting them would read the trees twice.
8. **Call sites in the new tree only.** `CHANGED` (unless `nested_only`), `REMOVED` and `MOVED` symbols get references in the new tree, other than inside the definition itself. A SCIP change is matched by exact symbol in files that answer from SCIP, and by name in files that answer from tags. A tags change is matched by name in every file. Name matches are narrowed by language family (Python; TypeScript, TSX and JavaScript; Go; Rust) and by kind. Calls match functions and methods, and type references match types. Tags record no references to fields, variables, constants or modules, so those are `NOT_TRACKED`. `name_definitions` counts the definitions a by-name match could also be. Bounds: 10 call sites listed per change by default (at most 100); counts stop at 10,000 candidates (`CAPPED`); 1,000 changes per page are looked up (`PAGE_LIMIT`); a file's call sites are dropped from its last change backwards when its message would pass 3 MiB.
9. **A row-level reference query.** `Store::find_rows` reads only the matching occurrences and each one's enclosing definition's symbol, never a whole file's occurrences. `ListSymbols` by name and symbol moves onto it.
10. **Expansion by diffing definitions.** `expand_symbols` returns only the files with a matching change, and as their hunks only those definitions' lines. Those are diffed against each other and numbered in file lines, so they agree with the file's own diff. An entry matches a symbol, a suffix of one beginning after a descriptor boundary, or a display name.
11. **Plain `reef diff` JSON unchanged.** The CLI's `Diff` gains `symbol_summary` and each file `symbols` only when they were asked for. R2.17's baseline and existing consumers read `reef diff` as it is.
12. **The MCP budget rises to 20,250 bytes.** `get_diff` gains `format: "symbols"` and `expand`, the same for every harness. Tightening `get_diff`'s own texts paid for most of it, and `tools/list` is 20,048 bytes. The symbols view is the input whose purpose is fewer tokens per answer. Trimming other tools to fit would make them less clear for every agent.

## Consequences

- An agent asking "what changed" gets definitions, not lines, and where they are used, and can ask for one definition's lines.
- Tags call sites are by name. Common names over-match, and calls through aliases, interfaces or dynamic dispatch are missed. Each call site says its source, and each change says how many definitions share its name. R3.18 must treat tags call sites as a hint.
- A renamed symbol is a removal and an addition: the digest covers the name. A move that also changed the body, under an identity that spells the file, is too.
- Matching re-reads the whole diff's definitions on every page. `docs/benchmarks.md` has the cost, and a cache between pages is added only if it shows.
- `tools/list` costs every session 48 bytes more than before.

## Alternatives considered

- **A `GetSymbolDiff` RPC.** It would repeat the revisions, paths, paging and rename logic, and still have to list the files it cannot answer for.
- **`DIFF_FORMAT_SYMBOLS`.** It would exclude counts and expansion, which combine with symbols.
- **Matching per file pair.** It misses package-scoped identities moving between files.
- **Reporting only the innermost changes.** It loses a parent's own edits; `nested_only` keeps them and still lets R3.18 skip the parent's callers.
- **Call sites in both trees.** The question is what still depends on the changed code.
- **A time budget for lookups.** Counting bounds are predictable; a time budget makes answers depend on load.
- **Clipping the file's hunks to a symbol.** Context and neighbouring edits leak in; diffing the definitions gives exactly their lines.
- **Trimming other tools to fit 20,000 bytes.** Every agent pays for less clear descriptions.
