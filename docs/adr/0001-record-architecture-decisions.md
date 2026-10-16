# 0001. Record architecture decisions

- Status: accepted
- Date: 2026-10-15
- Ticket: R0.07

## Context

[design.md](../design.md) makes Reef's foundational technology decisions in its [Technology choices](../design.md#technology-choices) table, with a one-line reason each. A table row records what was chosen but not what it was weighed against, what it commits us to, or what would make us change our minds. Without that, the reasons get lost or relitigated.

Several Phase 0 tickets (R0.01, R0.04, R0.08, R0.09) each need to record a decision while running in parallel, so they need a shared format and a numbering scheme that cannot collide.

## Decision

We will record architecturally significant decisions as architecture decision records, in the style Michael Nygard described, in `docs/adr/`.

- One Markdown file per decision, named `NNNN-kebab-title.md`, numbered with four zero-padded digits that are never reused or renumbered.
- Every ADR starts from [0000-template.md](0000-template.md), a trimmed MADR with the sections Status, Date, Ticket, Context, Decision, Consequences, and Alternatives considered.
- Statuses are `proposed`, `accepted`, `superseded by NNNN`, and `deprecated`. An accepted ADR is edited only to change its status or add a superseded note.
- Numbers are allocated in the ticket that calls for the ADR, before work starts.
- The index in [README.md](README.md) is maintained by hand and updated in the same PR as the ADR.

The decisions already made in design.md are backfilled as ADRs 0002 to 0006 and 0011. Other rows of the technology table get an ADR when the ticket that first depends on them lands.

## Consequences

- A contributor asking "why" about a dependency, protocol, or format has one place to look, and a reversal has to be written down as a new ADR that supersedes the old one.
- Writing an ADR is a cost on every significant decision. The template is kept short to keep that cost low.
- design.md and the ADRs can drift. Backfilled ADRs must not contradict design.md, and a later change to a decision updates both.
- The hand-maintained index can fall out of date; review catches that, since every ADR PR touches it.

## Alternatives considered

**Keep decisions in design.md only.** It already has the choices and one-line reasons. Rejected: one line per choice leaves out the alternatives and consequences, and a design document is rewritten in place, so the history of a decision disappears.

**adr-tools or a similar generator.** Rejected as unnecessary: the directory is plain Markdown and the numbering is assigned in tickets.

**Full MADR.** Rejected: its decision drivers and per-option pros and cons are more structure than these decisions need. Drivers fold into Context.
