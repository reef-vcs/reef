# Security policy

This is a stub. The full disclosure policy arrives with R6.05 ([docs/dev-phases.md](docs/dev-phases.md)).

## Supported versions

Reef is pre-1.0. Security fixes land on `main` and ship in the next release; no earlier release receives them, so run the latest.

## Reporting a vulnerability

Do not open a public issue. Report it privately through GitHub's private vulnerability reporting: on the repository, open the **Security** tab and choose **Report a vulnerability**. Only the maintainers can see the report.

Include what is affected, how to reproduce it, and the impact you expect. You will get an acknowledgement, and the fix and any advisory will be coordinated with you in the report's thread.

## Threat model

[docs/threat-model.md](docs/threat-model.md) says what Reef protects, from whom, and where it does not, with a findings table of every issue the reviews found and what became of it. In short: in Phase 1 one user id is one trust domain. Reef protects the user's state and workspaces from other local users, and acts safely on hostile repository content, but a workspace is not a sandbox: an agent running as the user can do anything the user can, including through the daemon's socket ([ADR 0027](docs/adr/0027-trust-boundaries-in-phase-1.md)). RX.01 wrote the model; RX.02 reviews evidence and identity (forged evidence, key handling, executor sandbox escape) and is the first external review.
