# Security policy

This is a stub. The full disclosure policy arrives with R6.05 ([docs/dev-phases.md](docs/dev-phases.md)).

## Supported versions

None yet. Reef is pre-release and nothing has been published for use, so no version receives security fixes. Fixes land on `main`.

## Reporting a vulnerability

Do not open a public issue. Report it privately through GitHub's private vulnerability reporting: on the repository, open the **Security** tab and choose **Report a vulnerability**. Only the maintainers can see the report.

Include what is affected, how to reproduce it, and the impact you expect. You will get an acknowledgement, and the fix and any advisory will be coordinated with you in the report's thread.

## Planned reviews

Two security reviews are on the roadmap: RX.01 covers workspace isolation (shared stores, daemon socket permissions, lease hijacking) and RX.02 covers evidence and identity (forged evidence, key handling, executor sandbox escape).
