# Conflicts

A conflict in Reef is jj's: when a merge or a rebase cannot resolve a path, jj records the commit anyway, with the conflict stored in its tree, and nothing blocks. Reef reads that stored conflict as data: every side, the commit each came from, who wrote it, and, on request, the hunks. This page says what a conflict object holds, how much of it to trust, and how to read one. The decisions are [ADR 0065](adr/0065-conflict-objects.md)'s.

Nothing here resolves a conflict. A resolution is a new snapshot, as any other edit: fix the file in a workspace (with or without jj's markers) or write it with `EditFiles`. Assigning conflicts to resolvers, policy tiers and attempt caps, and the conflict view are later work (R3.27, R3.28, R3.35).

## What jj stores

A conflicted commit's tree is a merge of *n* sides and *n − 1* bases, in the order side 0, base 0, side 1, base 1, …, side *n − 1*. A rebase that conflicts makes a two-sided conflict: the destination (side 0), the rebased commit's old parent (base 0), and the rebased commit itself (side 1). A merge of two parents is the same shape. A conflicted commit rebased onto a third change, or an octopus merge, makes wider ones.

Each term carries a label that jj writes into conflict markers, such as `kmnopqrs 1a2b3c4d "add the parser" (rebased revision)`: the commit's change id and commit id, shortened, its subject, and its role. Conflicts written before jj had labels, or by other clients, may have none, and any client can write any label.

A commit whose parent is conflicted carries the same conflict until someone resolves it. Reef tells these apart: a file is `inherited` when one of the commit's parents has the same conflict (the same file id) at that path, and a conflict's `introduced` counts the files that are not.

## A conflict object

`reef conflict show -r <rev>` (or `ConflictService.GetConflict`) reads one:

| Field | Means |
| --- | --- |
| `id` | `cfl_` and 32 hex digits: SHA-256 over the change id and each file's id. A describe, or an edit to a path that does not conflict, leaves it; a change to a conflicted path's sides changes it. |
| `kind` | `tree`: jj's stored conflict. Later tickets add conflicts jj does not store, such as a dependent of an undone operation (R3.26) or of a withdrawn dependency (R3.11). |
| `change` | The conflicted commit, as `reef show` prints it. |
| `arity` | The number of sides. |
| `terms` | Every term, in jj's order: `index`, `role` (`side` or `base`), jj's `label` verbatim, `origin` (the label's last parenthesized part, such as `rebase destination`), `source` (below), and for a side, `provenance` (below). |
| `change_provenance` | Who produced the conflicted change itself, summarized as for a side. A merge commit's owner wrote neither parent. |
| `files` | Each conflicted path: its `id` (SHA-256 of the path and its terms with repeated side and base pairs dropped, the same in a child that inherits it), `kind`, `inherited`, each term's value (`type`: `file`, `symlink`, `tree`, `submodule` or `absent`; the object `id`; `executable`), and with `--hunks` its hunks. |
| `files_total`, `introduced` | Conflicted paths in all, and those no parent carries. |
| `too_wide` | More than 64 terms: terms and sources are not reported, and files list paths only. |

A file's `kind` summarizes its terms: `content` (files everywhere, bits agree), `executable` (only the executable bit differs), `add-add` (no base has it, several sides add it), `modify-delete` (a side deletes it), `type` (a file against a symlink, directory or submodule; a file against a directory is one entry, not one per path below it), `other` (symlinks or submodules against their own kind).

## Where each side came from

jj does not record which commit a term came from; it records only the label. Reef uses the label to propose a commit and the content to decide:

1. **Linked commits**: the conflicted commit's parents, up to 16 earlier commits of its change (found in the operation log, at most 1,000 operations back) and their parents. A commit the term's label names (its commit id prefix resolved in the index, hidden commits included, kept only when the change id prefix matches) is linked when it is one of those or an ancestor of a parent. At most 32 candidates in all.
2. **The check**: a linked candidate is the term's source when its tree holds exactly the term's value at every conflicted path checked (the first 200 in path order), and the term has a value at one of them at least. The term's own label's linked commits are tried first.
3. **`attribution`**: `verified` when a candidate passed the check; `label` when none did and the label names a commit (that commit is the source, as the label declares it); `none` when the label names nothing that resolves.

What `verified` means: a commit related to this one holds this side's content at the conflicted paths. It does not prove that commit was the one merged: a related commit with the same content also matches. A commit only a label names, with no link to the conflicted commit, never verifies, and neither does a side that holds nothing at the conflicted paths (a deletion, or a base both sides add to), since any commit without the files would match it. A `label` source is only what the label's author wrote, and any client can write a label naming any commit (threat model S-127). **Route work on `verified` sources only**: R3.27's resolver assignment does, and only `verified` sides carry provenance.

After a rebase the rebased side's source is the change's own earlier commit, now hidden. It stays readable while jj indexes it; once operation GC has dropped the operations behind it, the side falls back to its label.

## Who wrote each side

For each `verified` side, the daemon reads the [provenance records](provenance.md) of the source's change (its newest 200), checking each again as every provenance read does, and reports (for at most 64 changes a response):

| Field | Means |
| --- | --- |
| `commit_record` | The record of the source commit itself, when the daemon recorded that commit (a workspace snapshot or an edit). A rebase or a describe writes none. |
| `latest_record` | The change's newest record that verifies, recorded at or before the source commit, else its newest that verifies: the agent, identity, task, model and instruction hash behind the side. |
| `sessions` | The sessions that recorded the change, newest first, at most 10, each with its count of this change's records and whether it has ended. |
| `sessions_total`, `records`, `invalid_records` | Sessions among the newest 200 records, records in all, and those of the 200 that do not verify. |

This is who recorded the side's change, not "the author": a change taken over by another session lists both, and a side from a person's git commit, or from a change whose records expired, has no records at all. The design's "task and instruction behind each" side is `latest_record`'s declared task reference and instruction hash; the instruction's text never reaches Reef. Bases have a source but no provenance; `reef provenance list --change <id>` reads theirs.

## Hunks

`reef conflict show --hunks` (`GetConflict` with `hunks`) splits each `content` and `add-add` file the way jj's markers do (line hunks, jj's merge options): the whole file in order, each hunk either resolved (`text`, where every term agrees or only one changed it) or conflicted (each term's `text`, `start_line` and `line_count`, one per term of the file's simplified merge, with `index` the term's index in the commit). Concatenating the resolved hunks and one term's conflicted texts gives that term's file with the other terms' non-conflicting changes applied, which is what jj's markers show for it, and `start_line` counts lines in that rebuilt file, not in the term's own.

A file is not split when a term has a NUL in its first 8,000 bytes (`binary`), or a term is over `max_file_bytes` (1 MiB by default, 16 MiB at most), its terms are over 32 MiB together, its hunks' text would pass 2 MiB, or it alone would pass the response's limit (`too_large`). A page with hunks ends after the file whose hunks pass 2 MiB in all. `reef file read -r <source commit> <path>` reads any term whole.

## Reading conflicts

```sh
reef conflict list                       # conflicted mutable changes: sides, attribution, agents, paths
reef conflict list --introduced          # without the children that only inherit a conflict
reef conflict list -r 'conflicts()'      # any revset
reef conflict show -r <rev>              # one conflict: terms, sources, provenance, per-term values
reef conflict show -r <rev> --hunks      # and each file's hunks, labeled by term
reef conflict show -r <rev> --policy-input > c.json && reef policy check --input c.json
```

`reef status` names `reef conflict show` under a workspace's conflicted paths, and a refused proposal or evidence for a conflicted change gives it as its `next`. `--policy-input` prints the conflict as a [policy](policy.md)'s `[[conflicts.rule]]` sees it (the change, every conflicted path, and per side the source's change and the identity and agent of its newest record), so a policy author can test a conflict tier; R3.28 applies tiers.

Through the API, `ListConflicts` reads the conflicted commits of a revset in its log order (`mutable() & conflicts()` by default; `introduced_only` leaves out inherited ones), each with its terms, sources and provenance and its first 20 paths, a page looking at no more than 10,000 commits (so a page may hold none, with a token to go on); `GetConflict` reads one with its files a page at a time (`paths` filters; every page repeats the terms). Both are READ: any caller of the socket, the web listener and the remote listener may use them. A revision without a conflict is `NOT_FOUND` with reason `NOT_CONFLICTED`. MCP has no conflict tools yet; an agent runs the CLI.

## Limits

| What | Limit |
| --- | --- |
| Terms reported per conflict | 64 |
| Candidates per conflict | 32, of them 16 predecessors, found within 1,000 operations |
| Labels, source descriptions (first line) | 1 KiB |
| Commits one `ListConflicts` page looks at | 10,000 |
| Changes whose provenance one response reads | 64 |
| Conflicted paths a source is checked at | 200 |
| Files per `GetConflict` page | 200 by default, 1,000 at most |
| Paths per `ListConflicts` entry | 20 |
| Conflicts per `ListConflicts` page | 50 by default, 200 at most |
| Bytes of a term hunks read | 1 MiB by default, 16 MiB at most |
| Hunk text per file, and per `GetConflict` page | 2 MiB |
| Response | 3 MiB |
