# 0010. DCO instead of CLA

- Status: accepted
- Date: 2026-10-16
- Ticket: R0.08

## Context

Reef is licensed under Apache 2.0 and targets the open ecosystem rather than one organization: the hub is a protocol anyone can host, and the goal is alternative implementations and many contributors. There is no company or foundation behind the project, so there is no legal entity that could hold the rights a contributor license agreement grants. Contributions are expected to come from many small contributors, each sending a few changes, and from agents working on someone's behalf. Any per-contributor paperwork is friction paid on every first contribution.

The project still needs a record of where each contribution came from. Provenance is Reef's own thesis: a change should carry evidence of where it came from rather than relying on who pushed it. A repository that cannot say who certified its own contents would contradict that.

This is the one contributor-process decision that is painful to change later, because it governs the rights in every commit already merged.

## Decision

We will use the [Developer Certificate of Origin](https://developercertificate.org/) 1.1. Every commit carries a `Signed-off-by:` trailer, added with `git commit -s`, naming a real person who certifies the DCO for that commit. Contributors keep their copyright and license their contributions under Apache 2.0, as section 5 of the license already provides for contributions submitted to the project.

Enforcement is the DCO GitHub App (`https://github.com/apps/dco`) installed on the `reef-vcs` organization, with its check required on `main`. There is no custom CI check. Installing the app and requiring its check are manual steps for the repository owner.

Agent-authored commits are signed off by the human who submits them, who thereby certifies the DCO on the agent's behalf. The agent is credited with a `Co-Authored-By:` trailer and named in the PR description (CONTRIBUTING.md).

This covers code, documentation, and everything else committed to this repository. It does not cover the governance documents, maintainer roles, or the security disclosure policy (R6.05).

## Consequences

- No copyright is assigned and no broader license is granted to anyone. Relicensing the project later would need the consent of every contributor whose code remains, or a rewrite of their parts. Apache 2.0 is permissive enough that we do not expect to need it.
- Contributing costs one flag on `git commit`. A missing sign-off is fixed with `git commit --amend -s` or `git rebase --signoff`, documented in CONTRIBUTING.md.
- Every commit names an accountable person, including commits written by agents, which matches the project's provenance principle.
- The DCO check depends on a third-party GitHub App. If it disappears, a short CI job checking each commit's trailers replaces it.
- We would revisit this if the project gains a legal entity that needs to hold rights, for example to relicense or to defend trademarks through a foundation.

## Alternatives considered

**CLA via CLA Assistant.** Contributors sign an agreement once, granting a project entity a broad license or assigning copyright. It would make relicensing possible, but it needs an entity to grant the rights to, which Reef does not have, and it adds a legal step before a first pull request. Agents cannot sign agreements, so every agent-authored PR would still depend on a human's signature. It lost on friction and on having no counterparty.

**Do nothing.** Rely on Apache 2.0 section 5 alone, with no per-commit statement. It is the lowest friction, but leaves the provenance of each contribution unrecorded, which contradicts the project's own thesis that changes carry evidence of where they came from.
