# 0061. Repository policy file

- Status: accepted
- Date: 2026-11-08
- Ticket: R3.07
- Amended by: [0066](0066-sigstore-keyless-session-keys.md) (attestor sets gain `issuer_kinds`; the input's key gains `issuer_kind`, `oidc_issuer` and `oidc_subject`)

## Context

[0006](0006-cel-for-policy.md) chose CEL, through cel-rust, for repository policy, and left the file's name to R3.07, with three notes: a policy is expressions over an input object, CEL cannot reach beyond its input so aggregates need precomputed inputs, and cel-rust's maturity is a risk to re-check. Every Phase 3 ticket since has left a question here: which signers count ([0055](0055-evidence-object-format.md), [0057](0057-operator-identities-and-session-keys.md)), how to exclude the submitting agent signing its own test run (the [design](../design.md)'s "What evidence proves"), what to do with artifacts' states ([0056](0056-evidence-artifact-store-and-retention.md)), provenance ([0058](0058-provenance-records.md)), job freshness ([0059](0059-executor-protocol.md)), and `reef-checks.toml` ([0060](0060-reference-executor.md)).

cel-rust today is the crate `cel`, 0.15.0, MIT: a tree-walking interpreter over a public AST, with custom functions, serde variables and the standard macros, eight breaking minor releases since June 2026, and no type checker and no cost limit. Its ANTLR parser recurses about a dozen frames per nesting level before its own depth limit applies.

The landing service (R3.13) needs data, not a boolean: which evidence to request from which signers, who must sign off, which tier a conflict gets.

## Decision

1. **The policy is `reef-policy.toml` at the root of a tree**: strict TOML, format version 1, whose tables declare owners, groups, attestor sets, required evidence, limits, forbidden paths, sign-off areas and conflict tiers, each with an optional CEL condition, plus free CEL rules. Unknown keys are errors; a later ticket's table is additive to version 1, and an older daemon reports a file using it `invalid_policy`.
2. **The policy in force is the destination's** (`trunk()` by default, the branch a change lands onto in R3.13), never the change's. A change to `reef-policy.toml` or `reef-checks.toml` needs `[owners]`' sign-off (`builtin:owners`, which cannot be removed); a change's own new policy must load (`builtin:policy-file`); no file is `no_policy` and one that does not load `invalid_policy`, and neither lets anything land automatically.
3. **Trusted attestors are named sets of bound keys**, selected by identity or group (names, or pinned ids), identity kind, actor (default `executor`), authority and presence, excluding by default evidence from the change's own provenance sessions and agent labels, with an optional CEL condition per object. Evidence is filtered (signature verified), annotated (`self`, `attestors`) and capped (every trusted object up to 10,000, others newest first up to 1,000) in Rust before CEL sees it.
4. **Required evidence, limits, forbidden paths and sign-offs are evaluated in Rust** and report data: missing and failed (kind, target, attestors) pairs, the sign-offs needed and given. The newest verdict by witness time decides a pair, and `error` never counts. A sign-off is `signoff` evidence, `pass`, from a `person` identity's key with actor `human`, counted by distinct identities.
5. **The input is precomputed, sorted and clock-free**: the change's commit and diff (renames contribute both paths), the evidence on its tree with each signer's identity at witness time and whether a job accepted it fresh, and the change's provenance records. The same policy bytes and input give the same decision and statuses.
6. **CEL is cel-rust's `cel`, pinned exactly** (`=0.15.0`, no default features but `regex`: no `chrono`, no extensions), used from three modules behind Reef's types, with Reef's own static check (variables, fields of known shape, functions, macros over maps, size and nesting) and a cost bound computed from the real input before anything runs (10^7 per expression, 5×10^7 per evaluation), both fixed in format 1. Nesting is bounded from the text before the parser sees it, and every load and evaluation runs on a thread with a 64 MiB stack.
7. **Every rule is evaluated and reported**, built-ins first; a rule that errors blocks; nothing allows by default.
8. **The evaluator is its own crate, `reef-policy`**, with no Reef crate but `reef-error` and no jj-lib, gRPC, tokio or SQLite, shared by the daemon, the CLI (`reef policy check` works offline) and the phase-four store. `PolicyService.EvaluatePolicy` is a READ RPC (API revision 20) that takes optional policy text; it never refuses anything. Enforcement is R3.13's.

## Alternatives

- **A pure `reef-policy.cel` expression**, as the roadmap row suggested: a boolean tells the landing service nothing about what to request or whom to ask.
- **TOML or YAML tables alone**: 0006 rejected them as not expressive enough for path-conditional rules; every table here takes a CEL `when`.
- **Rego through `regorus`**: the design's fallback; a second language for little gain at this size.
- **`cel-cxx`**: Google's checker and cost limits, at the price of a C++ toolchain in every build.
- **Reading the policy from the change, or from the daemon's configuration**: the change would judge itself, or the rules would not be in the repository and reviewed with it.
- **Trust written entirely in CEL over raw evidence**: no data for R3.13, and flooding and self-attestation to get right in every policy.
- **The oldest verdict, or any `fail`, deciding**: a retried flaky check could never pass on the same tree.
- **A wall-clock timeout instead of a cost bound**: not deterministic, and a thread running cel-rust cannot be stopped. **Limits set per daemon**: two daemons would disagree on one policy.
- **Allowing on error**: a policy that cannot tell must not let things land.

## Consequences

- [docs/policy.md](../policy.md) is the format, the input, the CEL environment, the static check, the cost bound and the result; R6.03 specifies them from it, with `crates/reef-policy/tests/corpus/` as conformance cases.
- `reef policy check|eval|init` and `reef schema policy`; `EvaluatePolicy` is not an MCP tool (`tools/list` is at its budget, #116): an agent runs `reef policy eval`.
- The threat model gains S-90 to S-97. On one uid, self-exclusion is a speed bump (S-92): a policy that must exclude agents trusts a `ci` identity and, after R3.43, verified presence.
- A cel-rust upgrade is its own change, run against the corpus. If cel-rust proves wrong, `cel-cxx` is the CEL fallback and Rego the design's.
- Later tickets add inputs (`change.state`, `stack`, `budget`, `batch`, the conflict object's fields) and tables (`[[transfer]]`, `[budgets]`) to format 1, additively. Daemons must be upgraded before policies that use them.
