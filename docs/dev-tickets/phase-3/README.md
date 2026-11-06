# Phase 3 tickets

Source of truth for scope: [`docs/dev-phases.md`](../../dev-phases.md) (Phase 3 tables) and [`docs/design.md`](../../design.md) (Data model, Landing, What evidence proves, CI load, Provenance, Durability and backup, Human interface). These files expand each Phase 3 row into an implementation brief. They are written for an implementing agent, not for humans: every decision that could be made in advance has been made here, and the brief says so. If a brief and `dev-phases.md` disagree on scope, the brief wins for the implementation and the disagreement is fixed in `dev-phases.md` in the same PR. Where a brief departs from `dev-phases.md`, the departure is listed under "Departures from dev-phases.md" below, and `dev-phases.md` was edited to match when the brief was written. Briefs are added as each ticket starts, so the table below fills in with them.

Phase 2 is complete and released as 0.2.0 to 0.4.0 (0.4.2 on jj-lib 0.47.0, 2026-11-05). What it left behind, and what every Phase 3 brief builds on:

- **The v1 API** (`reef.api.v1`, ADR 0029): every RPC classified `READ`, `WRITE` or `ADMIN`; `API_REVISION` and `API_DIGESTS` bumped on every schema change; the review checklist; `buf` lint and breaking checks. Writes coordinate with live workspaces (ADR 0031) and record `reef.*` attributes and the caller's `reef-agent` on every operation. Errors carry `ErrorDetail { kind, chain, hint, next }` and a reason constant documented on each RPC.
- **The thin clients**: `reef-client` (one method per RPC, page followers, `json` and `brief` types), the `reef` CLI under ADR 0025 (never interactive, `hint` and `next`, brief JSON on a pipe by ADR 0045), `reef mcp` (ADR 0035; every v1 method is a tool or in `NOT_TOOLS`, with `tools/list` under a byte budget, #116), `reef schema` (ADR 0050; every command has an `OUTPUTS` row), and the Python and TypeScript SDKs generated from the descriptor set (ADR 0054).
- **Live state**: the operation log subscription and broker (ADR 0036, 0038), presence (ADR 0037), replay (ADR 0046), the history and symbol indexes (ADR 0042, 0044), the status index (ADR 0048).
- **Listeners**: the loopback web listener (ADR 0047) and the remote listener with scoped tokens (ADR 0049). A new WRITE RPC gets a row in `remote/rules.rs`, and the threat model's listener requirements hold for every RPC.
- **The daemon's database**: `reef.sqlite3`, schema 6 at 0.4.2, numbered migrations that are never edited, every row keyed by repository (ADR 0003), copied whole by backups (ADR 0020).
- **The trust model** (`docs/threat-model.md`, ADR 0027): one uid is one trust domain; RX.02 is the first external review, and Phase 3 is where identity and signatures enter it.
- **Objects** are jj's git objects with SHA-1 identities until Phase 4 (ADR 0011), so trees and commits are git object ids.

## Tickets

| ID | File | Size | Depends on | Unblocks |
| --- | --- | --- | --- | --- |
| R3.01 | [R3.01-evidence-object-format.md](R3.01-evidence-object-format.md) | M | R2.03 | R3.02, R3.03, R3.05, R3.07, R3.19 |
| R3.02 | not yet briefed (Evidence artifact store) | M | R3.01 | none listed |
| R3.03 | not yet briefed (Session keys and operator identity) | L | R3.01 | R3.04, R3.06, R4.09, R4.10 |
| R3.04 | not yet briefed (Provenance records) | M | R3.03 | R3.25, R3.37 |
| R3.05 | not yet briefed (Executor protocol) | M | R3.01, R2.01 | R3.06, R6.03 |
| R3.06 | not yet briefed (Trusted executor reference implementation) | L | R3.05, R3.03 | R3.14, R3.22 |
| R3.07 | not yet briefed (Policy file and CEL evaluation) | M | R3.01 | R3.08, R3.13, R3.20, R3.24, R3.28, R6.03 |
| R3.08 | not yet briefed (Policy simulation) | S | R3.07 | none listed |
| R3.09 | not yet briefed (Change lifecycle states) | M | R2.03 | R3.10, R3.12, R3.13, R3.31, R4.07 |
| R3.10 | not yet briefed (Stacks) | M | R3.09 | R3.11, R3.24 |
| R3.11 | not yet briefed (Dependent change lifecycle) | M | R3.10, R3.13 | none listed |
| R3.12 | not yet briefed (Comments as instructions) | M | R3.09, R2.13 | R3.32 |
| R3.13 | not yet briefed (Landing service, local) | L | R3.07, R3.09 | R3.11, R3.14, R3.16 |
| R3.14 | not yet briefed (Batching and bisect) | L | R3.13, R3.06 | R3.15, R3.18, R3.20, R3.21, R3.34, R4.11 |
| R3.15 | not yet briefed (Evidence transfer rules) | S | R3.14 | none listed |
| R3.16 | not yet briefed (Landed history projection) | M | R3.13 | R3.17, R4.16 |
| R3.17 | not yet briefed (Degraded landing to a git remote) | M | R3.16 | R4.14 |
| R3.18 | not yet briefed (Minimal check set computation) | L | R3.14, R2.15 | none listed |
| R3.19 | not yet briefed (Per-target evidence) | M | R3.01 | R3.22 |
| R3.20 | not yet briefed (Compute budgets in policy) | M | R3.07, R3.14 | none listed |
| R3.21 | not yet briefed (Landing queue priority) | S | R3.14 | none listed |
| R3.22 | not yet briefed (Behavioral diff evidence kind) | M | R3.06, R3.19 | none listed |
| R3.23 | not yet briefed (Test changes in the proposal view) | M | R3.31 | none listed |
| R3.24 | not yet briefed (Independent test authorship rules) | S | R3.07, R3.10 | none listed |
| R3.25 | not yet briefed (Conflict objects in the API) | M | R3.04 | R3.26, R3.27, R3.35, R5.01 |
| R3.26 | not yet briefed (Agent-scoped undo) | M | R3.25, R2.03 | none listed |
| R3.27 | not yet briefed (Resolver assignment and queue) | M | R3.25 | R3.28 |
| R3.28 | not yet briefed (Resolution tiers and attempt caps) | M | R3.27, R3.07 | R3.29, R3.30 |
| R3.29 | not yet briefed (Intent conflict escalation) | M | R3.28 | none listed |
| R3.30 | not yet briefed (Backpressure) | S | R3.28 | none listed |
| R3.31 | not yet briefed (Proposal view) | L | R3.09, R2.15, R2.18 | R3.23, R3.33 |
| R3.32 | not yet briefed (Help requests from agents) | S | R2.13, R3.12 | R3.33 |
| R3.33 | not yet briefed (Triage inbox) | M | R3.31, R3.32, R2.09 | R3.39, R3.40, R4.13 |
| R3.34 | not yet briefed (Landing pipeline view) | M | R3.14, R2.18 | none listed |
| R3.35 | not yet briefed (Conflict view) | M | R3.25, R2.18 | none listed |
| R3.36 | not yet briefed (Map view with conflict warning) | L | R2.10, R2.18 | none listed |
| R3.37 | not yet briefed (Audit and explain) | M | R3.04, R2.16 | R3.38 |
| R3.38 | not yet briefed (File browser with time axis and symbol search) | M | R2.18, R2.14, R3.37 | none listed |
| R3.39 | not yet briefed (Desktop shell) | L | R3.33 | R3.40, R3.41 |
| R3.40 | not yet briefed (Mobile shell and push notifications) | L | R3.39, R3.33, R2.08 | R4.13, R4.17 |
| R3.41 | not yet briefed (Desktop shell on Linux and Windows) | L | R3.39 | none listed |
| RX.02 | not yet briefed (Security review of evidence and identity) | L | Phase 3 (cross-cutting) | none listed |
| RX.03 | not yet briefed (Opt-in telemetry) | M | Phase 3 (cross-cutting) | none listed |

## Departures from dev-phases.md

These were decided while writing the briefs. `dev-phases.md` was edited to match in the same change.

- **R3.01's "attestor" is the signing key and its "snapshot hash" is the snapshot's tree hash** (2026-11-05). A field in which the producer names itself proves nothing, so the attestor is the key that signed the envelope, and R3.03 binds keys to identities. Evidence binds to the tree, as the design's Landing and CI load sections say, not to the commit, which the data model row's wording suggested. The design's data model row is reworded in R3.01's PR.
- **R3.02 also owns the retention of evidence objects** (2026-11-05). R3.01 stores evidence with no deletion except `repo forget`, and a retention rule needs a walk over reachable trees that would push R3.01 past M.
