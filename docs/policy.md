# Repository policy

A repository's policy is `reef-policy.toml` at the root of its tree: which signers' evidence counts, which evidence a change needs, how large a change may be, which paths no change may touch, which areas need a person's sign-off, and how conflicts are resolved. Reef evaluates it over one change and answers with a decision and a result per rule: what is satisfied, what is missing, and who must sign. This page is the format, version 1 ([ADR 0061](adr/0061-repository-policy-file.md)).

Evaluation decides; it never refuses anything. The landing service (R3.13) is what will refuse to land a change the policy does not allow. Until then, `reef policy eval` tells you, or an agent, whether a change would land and what it lacks.

```sh
reef policy init                      # a starter reef-policy.toml in this directory
reef policy check                     # its keys, references, patterns and CEL, with no daemon
reef policy eval                      # the policy on trunk() over @: allow, block, or why not
reef policy eval -r @- --require-allow    # exit 5 unless allowed
reef policy simulate --limit 50       # what this file would have blocked of the last 50 landings
```

## Where the policy comes from

A change is never judged by its own policy.

- **The policy in force is the one on the destination's tree**: `trunk()` by default (`main`, `origin/HEAD`'s branch, `master` or `trunk`), the branch it lands onto once R3.13 lands changes. `reef policy eval --policy-revision REV` reads another revision's, and `--policy-file FILE` evaluates a file's text, for trying a policy out; `reef policy simulate` tries one over recent landings ([Simulating a policy](#simulating-a-policy)).
- **A change to `reef-policy.toml` or `reef-checks.toml` needs the owners' sign-off.** The built-in rule `builtin:owners` applies whenever a change touches either file, at the tree's root, and needs `[owners]`' sign-off, as a `[[signoff]]` area would. It cannot be removed: a policy that a change could switch off for itself would be no policy. `reef-checks.toml` is included because it declares what the reference executor runs for a check ([executor.md](executor.md)), so a change to it changes how a tree is judged.
- **A change's own new policy must load.** `builtin:policy-file` applies when a change touches `reef-policy.toml` and keeps it: the new file must parse and pass the static check, so the destination never receives a policy that blocks everything. Deleting the file is allowed, with the owners' sign-off.
- **No file is `no_policy`**, and a file that does not load is `invalid_policy`, with every problem it has. Neither lets anything land automatically.
- **The first policy reaches the destination outside the gate**: there is nothing to judge it by. Commit it by hand after `reef policy check`.

Every evaluation names the bytes it evaluated: the policy revision, its commit, the file's blob id and the SHA-256 of its bytes. R3.13 pins them with each landing.

## The file

Strict TOML: an unknown key or table is an error with its line and column, as `reef.toml`'s are. Every table but `version` and `[owners]` is optional.

```toml policy
version = 1                                # required: the format version this file is written for

[owners]                                   # required: who signs off a change to the policy files
groups = ["maintainers"]                   # groups and identities; at least one between them
identities = []
count = 1                                  # distinct identities needed; default 1
presence = "none"                          # none | user_verified (R3.43); default none

[groups]                                   # named sets of identities, by name or by id (idn_...)
maintainers = ["alice", "idn_8f3a0c2d91e4b675"]

[attestors.ci]                             # a named set of signers whose evidence counts
identities = ["ci-main"]                   # names or ids; with `groups`, at least one entry
groups = []
identity_kinds = ["ci"]                    # person | ci; default either
actors = ["executor"]                      # human | agent | executor; default ["executor"]
authorities = []                           # authority key ids; empty: any authority this daemon verifies
presence = "none"                          # the least presence accepted; default none
self = "exclude"                           # exclude | allow: evidence from the change's own session or agent
when = 'e.fresh && e.artifacts.exists(a, a.state == "stored")'   # optional CEL over `e`

[[require]]                                # evidence the change needs
name = "tests"                             # unique in the file: [a-z0-9][a-z0-9_-]{0,63}
paths = ["crates/**"]                      # applies when a touched path matches; absent: every change
except = ["crates/**/*.md"]
kinds = ["test", "lint"]                   # each needs a passing object from one of `attestors`
targets = []                               # each kind needs these targets; empty: the whole check
attestors = ["ci"]
when = 'change.lines_added > 0'            # optional CEL over the input
message = "crate changes need CI's tests and lint"   # optional; one is generated otherwise

[limits]                                   # each key optional
max_files = 400                            # touched paths
max_lines = 4000                           # lines added plus removed
max_file_size = "5MiB"                     # the new side of any added or modified file
except = ["Cargo.lock", "web/package-lock.json"]   # not counted

[[forbid]]                                 # paths no change may touch
name = "secrets"
paths = ["**/*.pem", "secrets/**"]
except = []
message = "no keys in the tree"

[[signoff]]                                # areas that need a person's sign-off
name = "ci-config"
paths = [".github/**", "scripts/release*"]
groups = ["maintainers"]
count = 1
presence = "none"
kinds = ["signoff"]                        # evidence kinds that count as a sign-off; default ["signoff"]

[conflicts]                                # tiers and attempt caps; R3.28 applies them
tier = "approve"                           # land | approve | ask; default approve
attempts = 3                               # 1 to 20; default 3

[[conflicts.rule]]
paths = ["docs/**"]
tier = "land"
attempts = 5

[[rule]]                                   # anything else, in CEL
name = "provenance"
when = 'change.files.size() > 0'           # optional: false makes it not apply
expr = 'provenance.all(r, r.signature == "valid" && r.key.status == "bound")'
message = "every commit of the change has a signed, bound provenance record"
```

- **References are checked at load.** Every group an entry names exists in `[groups]`, every attestor set a `[[require]]` names exists, and every identity entry is an identity name (`[a-z0-9][a-z0-9._-]{0,62}`) or an id (`idn_` and 16 hex digits). Names are unique across `[[require]]`, `[[forbid]]`, `[[signoff]]` and `[[rule]]`; `limits` is the `[limits]` table's.
- **Sizes** are `"5MiB"`, `"512KiB"`, `"1GiB"` or a number of bytes.
- **Every problem is reported**, each with its rule, key, line and column, except that a TOML syntax or schema error stops loading at the first.

### Path patterns

Globs over repository-relative paths with `/` separators: `*` and `?` never cross `/`, `**` does, `{a,b}` alternates, and a pattern that names a directory (`docs`) matches only that path, not its contents (`docs/**` does). A pattern that does not compile is a load error, and so is one starting with `/`.

Matching is **ASCII case-insensitive** everywhere, so `Secrets/key.pem` is caught by `secrets/**`: a macOS checkout writes both to one directory. Unicode is not folded (APFS treats the NFC and NFD forms of a name as one); a repository that must not be fooled that way restricts paths to ASCII with a rule (the recipe below).

A table applies to a change when some touched path is matched by its `paths` (any path when it has none) and not by its `except`; a table with neither applies to every change. A rename touches its old and new paths.

## Trusted attestors

An `[attestors.<name>]` set decides whose evidence counts, per evidence object, in this order:

1. The key is `bound`: certified by an authority this daemon verifies, witnessed within the key's window and before every revocation that applies ([identity.md](identity.md)). Unbound, expired, revoked and invalid keys never count.
2. The key's identity is one of `identities`, or a member of one of `groups`: matched by id for `idn_...` entries and by the certificate's name otherwise.
3. `identity_kinds`, `actors` (default `["executor"]`) and `authorities` (64-hex authority key ids; empty for any authority this daemon verifies), when given.
4. The key's presence is at least the set's (`none` < `user_verified`). Every key today has `none`, so a set that asks for `user_verified` matches nothing until R3.43.
5. Unless `self = "allow"`, the object did not come from the change itself (below).
6. `when`, a CEL expression over `e` (the object) and `groups`, holds. One that errors on an object (or costs too much) leaves that object out and makes `builtin:input` an error, which blocks: the object could be the newest verdict, and leaving it out must not let an older one decide. The diagnostics name the set and the object.

The names of the sets an object satisfies are `e.attestors`, which free rules can use.

**What `self` excludes.** An object is the change's own when its key was certified for a session that also recorded one of the change's provenance records ([provenance.md](provenance.md)), when its key's agent label, with the same identity, is the agent of one of those records, or when the `reef-agent` that added it is. That implements the design's "excludes the submitting agent signing its own test run" as far as Reef can know it. On one uid it is a speed bump, not a wall: an agent that wants to can obtain a key for its user's person identity with any declared actor, from another session, and add evidence under another agent label ([threat model](threat-model.md), S-59, S-92). What keeps an agent from attesting its own work is trusting an identity whose keys it cannot get: a `ci` identity, whose keys need its remote token, and, after R3.43, `presence = "user_verified"` for sign-offs.

**Names and ids.** Identity names are never reused within one daemon's database, but restoring an old backup can bring a name back with a new id. A policy that must not be fooled pins ids, and authorities, rather than names (S-93).

**Annotations** are the attestor's own claims, unverified: `e.annotations.sandbox.kind == "bubblewrap"` trusts what a trusted executor says about itself (S-97).

## Required evidence, limits, forbidden paths and sign-offs

These tables are evaluated by Reef itself, not compiled to CEL, so their results carry data: R3.13 requests the missing evidence from the named attestor sets, and the proposal view shows who must sign.

- **`[[require]]`**: for each kind, and each of `targets` (or the whole check, no target, when empty), the objects of that kind and target from a named set decide, and **the newest verdict by witness time wins**: the latest `pass` or `fail` among them is the requirement's outcome. `error` objects never count: R3.01 defines `error` as no verdict. So a flaky run that failed and then passed on the same tree is satisfied, and a pass followed by a fail is not. No object is **missing**; a newest `fail` is **failed**, with its id. A per-target object never satisfies a whole-check requirement: how targets add up to a whole check is R3.19's.
- **`[limits]`**: `max_files` over the touched paths, `max_lines` over lines added plus removed, `max_file_size` over each added or modified file's new side, all after `except`. A binary file, or one too large to count (over 16 MiB), counts no lines. One rule, `limits`, reports every limit passed and the files over the size limit.
- **`[[forbid]]`**: unsatisfied when a touched path matches, listing up to 20 and the total.
- **`[[signoff]]`** and **`builtin:owners`**: a sign-off is evidence on the change's tree of one of `kinds` (default `signoff`), `pass`, from a bound key with actor `human` certified for a `person` identity in `groups` or `identities`, with at least `presence`; `count` distinct identities are needed. Self-exclusion does not apply: a person signs off their own agents' work. Sign with `reef evidence add --kind signoff --result pass --actor human` today; R3.09 and R3.31 will sign from the proposal view, and R3.43 adds verified presence. A sign-off is on one exact tree; carrying it across a later edit is R3.15's.

## Free rules

`[[rule]]`'s `when` (optional) and `expr` are CEL over the whole input. `when` false: `not_applicable`. `expr` true: `satisfied`; false: `unsatisfied` with the rule's `message`; any other value, or any error at run time: `error`, with the error's text. The `when` of a `[[require]]`, `[[forbid]]` or `[[signoff]]` works the same way: one that errors, or yields anything but a boolean, makes its rule `error`.

## The input

What a policy sees about one change, built by the daemon. `reef schema policy input` prints it as JSON Schema; `reef policy eval --input-json` prints a real one, and `reef policy check --input FILE` evaluates a policy over a file of it, offline. Strings for enumerations, integers (never unsigned) for counts and times in milliseconds since the Unix epoch, and no `null`: an absent value is an empty string, zero or an empty list.

| Variable | What |
| --- | --- |
| `change` | `id`, `commit`, `tree`, `parents`, `description`, `author.name`, `author.email`, `empty`, `conflicted`; `paths`, every touched path, sorted (a rename contributes both); `files`, each `{path, old_path, status, binary, too_large, lines_added, lines_removed, size}` with `status` one of `added`, `modified`, `deleted`, `renamed`, `copied`; `lines_added`, `lines_removed` |
| `evidence` | the evidence on `change.tree` whose signature verifies, by witness time then id: `id`, `kind`, `result` (`pass`, `fail`, `error`), `target`, `started_at`, `finished_at`, `added_at` (the witness time), `added_by_agent`, `artifacts` (`{name, sha256, media_type, state, size}`, `state` one of `stored`, `collected`, `absent`), `annotations`, `key` (below), `fresh` and `job` (whether an executor job accepted it fresh, R3.05), `self`, `attestors` |
| `provenance` | every provenance record of `change.id`: `id`, `commit`, `tree`, `workspace`, `session`, `sequence`, `kind`, `agent`, `recorded_at`, `model`, `harness`, `task`, `instruction_sha256`, `client_session`, `signature` (`valid`, `invalid`), `key` |
| `groups` | the policy's `[groups]`, from name to members as written |

A `key` is `{id, status, identity, identity_id, identity_kind, actor, agent, session, presence, custody, authority}`: `status` one of `unbound`, `bound`, `expired`, `revoked`, `invalid`, judged at the object's witness time; `identity_kind` `person` or `ci`; `actor` `human`, `agent` or `executor`.

- **Evidence is filtered before CEL, not trusted by it.** Objects whose signature does not verify are counted, not shown (`evidence_ignored`). Every object some attestor set or sign-off rule accepts is kept, up to 10,000 (more is `builtin:input`'s error, which blocks); the rest are kept newest first up to 1,000, so junk on a tree cannot push trusted evidence out of view (S-94). The daemon reads at most the newest 20,000 objects of a tree, and an object's annotations only when they are at most 16 KiB as JSON (otherwise `{}`, so a condition over them fails and blocks).
- **Provenance**: every record of the change, up to 10,000; more is `builtin:input`'s error.
- **No clock.** Nothing in the input is "now", and identity status is judged at each object's witness time, so an evaluation depends only on the policy's bytes and the input.
- **Limits**: at most 100,000 touched paths; a larger change is `builtin:input`'s error.
- **What later tickets add**, each a new field the static check learns: `change.state` (R3.09), `stack` (R3.10, for R3.24), the `conflict` object's other fields (R3.25 to R3.28), `budget` (R3.20) and `batch` (R3.14). A policy that names a field this Reef does not know is `invalid_policy`, so a policy written for a later Reef fails closed on an earlier one: **upgrade daemons before policies**.

## CEL

Conditions and rules are [CEL](https://cel.dev), on [cel-rust](https://github.com/cel-rust/cel-rust)'s `cel` 0.15.0: the standard library and macros (`has`, `all`, `exists`, `exists_one`, `map`, `filter`), optional syntax (`a.?b`), and no extensions. There are no `duration` and `timestamp` types: times are integers. `matches` uses a linear-time regex engine; Reef compiles each pattern once per evaluation, with at most 256 KiB of compiled program and at most 64 distinct patterns (a literal pattern is checked when the file loads). Reef's glob functions take at most 256 patterns a list and 64 distinct lists an evaluation.

Reef adds four functions, all pure:

| Function | Means |
| --- | --- |
| `glob(path, pattern)` | `path` matches the glob `pattern`, by the file's glob rules |
| `any_glob(paths, patterns)` | some path matches some pattern |
| `all_glob(paths, patterns)` | every path matches some pattern |
| `matching(paths, patterns)` | the paths that match, in order |

Each expression sees some of the variables: a `[[require]]`, `[[forbid]]`, `[[signoff]]` or `[[rule]]` expression sees `change`, `evidence`, `provenance` and `groups`; an attestor set's `when` sees `e` and `groups`; a `[[conflicts.rule]]`'s `when` sees `conflict` (`{change_id, paths, sides: [{change_id, identity, agent}]}`) and `groups`.

### The static check

cel-rust has no type checker, so Reef checks every expression when the file loads and refuses, with its line and column:

- an identifier that is neither a variable of the expression nor bound by an enclosing macro;
- a field selected on a value of known shape (the input's, followed through selections and through the elements of a macro's list) that the shape does not have, such as `change.pahts` or `change.files.all(f, f.lines)`. Below `annotations`, nothing is checked;
- a call to a function that is neither the standard library's nor Reef's, and a literal pattern that does not compile;
- a macro over a map (`groups`, `annotations`, a map literal), whose iteration order cel-rust leaves undefined, and `map` or `filter` over anything not known to be a list (`dyn(...)`, a value under `annotations`), since their results expose that order;
- a message literal (`Foo{a: 1}`);
- an expression over 8 KiB of source, over 1,000 nodes once its macros are expanded, or nested deeper than 48 (brackets, unary operators and `?` together, counted from the text).

What the check cannot see, such as a wrong type or a missing key under `annotations`, fails at run time as the rule's error, which blocks.

### The cost bound

cel-rust tracks no cost either, so before each expression runs Reef computes an upper bound on what it costs over this input, exact about the input's sizes and the same on every run:

- every node costs 1. Each node also has a size bound: the longest list and the widest string it can be or hold, read from this input (the longest list and widest string at a path, at any depth) or worked out (`a + b` adds both; `map` holds what its step produces). `+` costs the size of its result; comparisons, `in`, `contains`, `startsWith`, `endsWith`, `matches` and Reef's functions cost their operands' sizes; a string's size is its bytes over 4, a list's its length. So a value that doubles at each step is charged for doubling;
- a macro costs its list, its accumulator and its result once, plus the list's bound times the cost of its loop condition and step (appending to `map`'s or `filter`'s result costs the element, not the list);
- a list's bound is: a literal's length; for a selection from a variable or a macro's element of known shape, the longest list at that path in this input (`change.paths`: its length; `e.artifacts`: the most artifacts that object has, since an attestor set's `when` is bounded over the one object it runs on); for `map`, `filter` and `matching(x, ...)`, its list's bound; for `a + b`, the sum; for `c ? a : b`, the larger; numbers and booleans have none; and for anything else, the input's total list elements and widest string plus the expression's literal ones.

An evaluation waits its turn behind at most three others in one daemon. An expression over **10,000,000** is not run and is its rule's error ("cost 3.2e9 over 1e7"); the sum over one evaluation is bounded at **50,000,000**, past which every remaining expression is an error. These numbers are part of format 1, so two implementations agree on what is too costly. A rule over an unknown-shape list is charged the whole input: write path rules with the tables or `any_glob` rather than nested macros over `change.paths`.

## The result

| Decision | Means |
| --- | --- |
| `allow` | every rule is satisfied or does not apply |
| `block` | some rule is unsatisfied or errored |
| `no_policy` | the policy revision has no `reef-policy.toml` |
| `invalid_policy` | the file does not load; `diagnostics` has every problem |

Every rule is evaluated and reported, so a blocked change sees every reason at once: the built-in rules first (`builtin:input`, the input was within its limits; `builtin:conflicts`, the change's tree has no conflict; `builtin:policy-file`; `builtin:owners`), then the file's in file order. A rule result has its `status` (`satisfied`, `unsatisfied`, `not_applicable`, `error`), a `message`, the touched `paths` it is about (at most 20, and `paths_total`), the `missing` and `failed` evidence (each `{kind, target, attestors, evidence_ids}`), the `signoff` it needs and has, and the `evidence_ids` that satisfied it. **Everything fails closed**: a rule that errors blocks, and nothing means "allow because the policy could not tell". `reef schema policy evaluation` prints the shape.

**Conflict tiers.** `[conflicts]` and `[[conflicts.rule]]` are not part of a change's evaluation. For a conflict, the first `[[conflicts.rule]]` whose `paths` match one of the conflict's paths (or that names none) and whose `when` holds gives the tier and attempt cap; else `[conflicts]` does (`approve` and 3 without the table). A `when` that errors is the error, which R3.28 treats as the strictest tier, `ask`. `reef policy check --input` with `{"conflict": {...}}` prints the tier.

## Simulating a policy

Before a policy change lands, `reef policy simulate` shows what it would have done to recent history: it evaluates a proposed policy over recent landings, beside the policy in force, and reports which landings it would have blocked and why ([ADR 0062](adr/0062-policy-simulation.md)).

```sh
reef policy simulate                                  # ./reef-policy.toml over the newest 20 landings
reef policy simulate --policy-revision @ --limit 200  # the policy a change proposes, over 200
reef policy simulate -r 'trunk()~30..trunk()' --all   # every landing of a window, allowed ones too
reef policy simulate --no-baseline -o json            # the proposed policy's decisions alone
```

- **The proposed policy** is `./reef-policy.toml` by default, read by the CLI and sent as text; `--policy-file FILE` names another, and `--policy-revision REV` reads the one at a revision, such as a change that edits it. A proposed policy that does not load is an error (exit 2) listing every problem, as `reef policy check` lists them; nothing is evaluated.
- **A landing**, until the landing service records landings (R3.13), is a commit that reached the destination: every ancestor of `trunk()` but merges and the root, newest first (`::trunk() ~ root() ~ merges()`), each judged as its own change exactly as `reef policy eval -r <commit>` judges it, with the evidence on its tree and its provenance as they are now. A merge is left out because what R3.07 judges of it is its diff against its merged parents, its conflict resolution; the commits it merged are in the window. `-r REVSET` names any other window: `-r 'first_ancestors(trunk(), 50)'` for the first-parent line, `-r 'ancestors(trunk()) & committer_date(after:"2 weeks ago")'` by time. Once R3.13 records landings, with the evidence they used, they replace this default.
- **The baseline** is the policy at `trunk()` (`--baseline-revision REV` for another, `--no-baseline` for none), the same for every landing, so the report says what the proposed change to the policy changes. Each landing's outcome compares the two decisions, where blocked means anything but `allow`:

  | Outcome | Proposed policy | Baseline |
  | --- | --- | --- |
  | `newly_blocked` | blocks | allows |
  | `still_blocked` | blocks | blocks, or does not load |
  | `newly_allowed` | allows | blocks, or does not load |
  | `still_allowed` | allows | allows |
  | `blocked`, `allowed` | blocks, allows | no policy, or `--no-baseline`: nothing to compare with |

  The landings in the window did land; a baseline with no policy judged none of them, so it compares with nothing rather than "blocking" them all.
- **Missing evidence on history.** Each blocked landing lists its unmet rules, and `missing_evidence_only` marks a landing blocked only because evidence a `[[require]]` needs is missing (no trusted object at all, not a trusted failure). History usually lacks evidence: evidence binds to a tree, a forge's squash merge makes a new tree that no check ran on, and evidence on an old trunk commit expires after `[evidence] retention` ([evidence.md](evidence.md#retention)). So the summary counts those landings apart, and the other reasons (limits, forbidden paths, sign-offs, rules, failed evidence) are what a simulation over history tells you. The policy runs as written; nothing pretends the evidence was there, so each decision is the one `reef policy eval` would give. Identity status is judged at each object's witness time, as always, so a key revoked since with an earlier `--since` no longer counts.
- **Bounds.** The daemon answers a page at a time (`PolicyService.SimulatePolicy`, a READ call) and the CLI follows pages up to `--limit` landings (20 by default, at most 1,000). A page holds at most 100 landings (20 by default) and ends early after the landing whose evaluations, proposed and baseline, take its summed cost past 100,000,000 (twice one evaluation's bound), after the landing that takes the paths its landings touch past 200,000 or the evidence and provenance rows it reads past 50,000, or before its landings pass 3 MiB encoded. A landing too large for a page alone is `trimmed`: its full evaluations, and if need be its unmet rules' paths, are left out. A daemon reads at most two pages at once, each evaluation takes one of its four evaluation turns in turn, and later pages read the operation the first page read.
- **Output.** Text lists the landings the proposed policy blocks (`--all`: every landing) with each unmet rule's reason, then a summary ("20 landings: 7 would have been blocked (3 newly, 4 also by the baseline; 5 only for missing evidence); 13 allowed") and how many landings each rule blocked. JSON has every landing with both decisions, the outcome, the unmet rules and the cost, and `truncated` when `--limit` stopped short; `--evaluations` adds each landing's full evaluations. It exits 0 whatever it reports.

## Recipes

A solo developer trusting their own machine: evidence from this daemon's executor, signing for them, certified by this daemon's authority (`reef identity authority show` prints its key id).

```toml policy
version = 1

[owners]
identities = ["alice"]

[attestors.mine]
identities = ["alice"]
actors = ["executor"]
authorities = ["5f0c4a6e2b3d9817c6e5a4f3b2d1c0e9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3"]

[[require]]
name = "tests"
kinds = ["test"]
attestors = ["mine"]
```

A team trusting only its CI identity, and only with a stored log; sign-off from a maintainer on CI configuration; no keys in the tree.

```toml policy
version = 1

[owners]
groups = ["maintainers"]
count = 2

[groups]
maintainers = ["alice", "bob", "carol"]

[attestors.ci]
identities = ["ci-main"]
identity_kinds = ["ci"]
when = 'e.artifacts.exists(a, a.name == "test.log" && a.state == "stored")'

[[require]]
name = "checks"
except = ["docs/**", "**/*.md"]
kinds = ["build", "test", "lint"]
attestors = ["ci"]

[[signoff]]
name = "ci-config"
paths = [".github/**"]
groups = ["maintainers"]

[[forbid]]
name = "keys"
paths = ["**/*.pem", "**/id_rsa", "**/id_ed25519", "**/.env"]
```

Every commit of the change recorded with a bound, signed provenance record, and paths restricted to ASCII.

```toml policy
version = 1

[owners]
identities = ["alice"]

[[rule]]
name = "provenance"
when = 'change.files.size() > 0'
expr = 'provenance.size() > 0 && provenance.all(r, r.signature == "valid" && r.key.status == "bound")'
message = "every commit needs a signed, bound provenance record"

[[rule]]
name = "ascii-paths"
expr = 'change.paths.all(p, p.matches("^[ -~]+$"))'
message = "paths are ASCII"
```

Small changes land with less: tests only when code changed, and a size limit.

```toml policy
version = 1

[owners]
identities = ["alice"]

[attestors.local]
identities = ["alice"]

[[require]]
name = "tests"
kinds = ["test"]
attestors = ["local"]
when = '!all_glob(change.paths, ["docs/**", "*.md"])'

[limits]
max_files = 200
max_lines = 2000
max_file_size = "2MiB"
except = ["Cargo.lock"]
```

Conflicts in documentation resolve and land; anything touching the policy asks first.

```toml policy
version = 1

[owners]
identities = ["alice"]

[conflicts]
tier = "approve"
attempts = 3

[[conflicts.rule]]
paths = ["reef-policy.toml", "reef-checks.toml"]
tier = "ask"

[[conflicts.rule]]
paths = ["docs/**"]
tier = "land"
attempts = 5
```

## Upgrading

The format is versioned by `version`; this Reef reads 1 and reports any other as `invalid_policy` ("this Reef reads policy version 1"). Later tickets add tables (R3.15's transfer rules, R3.20's budgets) and input fields to version 1, additively; a daemon older than the policy then reports it `invalid_policy`, which blocks every landing. That is intended: a daemon must never land a change under rules it cannot read. Upgrade daemons before policies.

CEL runs on cel-rust pinned to an exact version, since each of its minor releases has changed its API and could change what an expression evaluates to. An upgrade is its own change that runs `reef-policy`'s corpus (`crates/reef-policy/tests/corpus/`).
