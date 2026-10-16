# 0006. CEL for repository policy

- Status: accepted
- Date: 2026-10-15
- Ticket: R0.07

Backfilled from design.md; decision predates this record.

## Context

Each repository declares a landing policy: trusted attestors, required evidence kinds, size limits, forbidden paths, sign-off areas, conflict resolution tiers, attempt caps, and compute budgets ([Landing](../design.md#landing), [CI load](../design.md#ci-load), [What humans do](../design.md#what-humans-do)). Policy is evaluated on every landing and in the editor's simulation of recent landings, so it must be fast. It decides what lands, so it must be auditable, and it must not be Turing-complete.

## Decision

We will express repository policy in CEL, evaluated with `cel-rust`.

- One policy file per repository, in the repository. Its name is settled in R3.07.
- The landing service evaluates it at landing, and policy simulation evaluates it against recent landings.
- Rego is the named fallback if policies grow large enough to outgrow CEL.

## Consequences

- A policy is a set of expressions over an input object, which the evidence and policy specification (R6.03) must document *(addition)*.
- CEL iterates only through bounded macros (`all`, `exists`, `map`, `filter`) over its input and cannot reach beyond it, so rules over data outside the input, such as budgets summed across other proposals, need host-provided functions or precomputed inputs *(addition)*.
- The maturity of `cel-rust` is a risk to re-check when R3.07 starts *(addition)*.

## Alternatives considered

**Rego with OPA.** More powerful, widely used for policy. Rejected for now: a heavier runtime and a separate language ecosystem *(addition: the reasons)*. It is the fallback.

**Starlark.** Familiar from Bazel. Rejected: an imperative scripting language of statements and mutation rather than an expression language, so a policy is harder to audit at a glance, and termination is guaranteed only in dialects that forbid recursion and `while` *(addition)*.

**TOML or YAML rule tables.** Simplest to read. Rejected: not expressive enough for path-conditional rules *(addition)*.
