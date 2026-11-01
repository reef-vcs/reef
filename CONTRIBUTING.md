# Contributing to Reef

Thanks for helping. This file covers how work is organized, how to set up, and what a pull request needs before it can merge. Participation is governed by the [code of conduct](CODE_OF_CONDUCT.md). Security problems go through [SECURITY.md](SECURITY.md), not public issues.

## Status

Reef is in Phase 2: typed API, MCP server, and live state. Phase 1 (leased copy-on-write workspaces) is complete and was released as 0.1.0, and the Phase 2 work so far as 0.2.0, for macOS and Linux; nothing is stable yet. The roadmap is broken into phases and tickets in [docs/dev-phases.md](docs/dev-phases.md), and the current phase's tickets have implementation briefs in [docs/dev-tickets/](docs/dev-tickets/phase-2/README.md). Open work is the Phase 2 rows not yet done and the issues labeled [`phase-2`](https://github.com/reef-vcs/reef/issues?q=is%3Aissue+is%3Aopen+label%3Aphase-2). Bugs, proposals, and tickets each have an issue form; open-ended questions belong in [Discussions](https://github.com/reef-vcs/reef/discussions).

## Setup

macOS and Linux are supported. Windows is not yet; whether and how to support it is ticket RX.05. To run Reef rather than build it, follow the [quickstart](docs/quickstart.md).

```sh
# Install rustup from https://rustup.rs. rust-toolchain.toml pins the toolchain,
# and rustup installs it on the first cargo command.
cargo install just --locked
just setup   # installs the git hooks and the cargo tools CI uses (nextest, deny)
just ci      # everything CI runs, in CI order
```

`just ci` checks the API schema with [buf](https://buf.build/docs/installation) (`brew install bufbuild/buf/buf`, or a release binary on Linux). Building never needs it, nor `protoc`.

`just` with no arguments lists every recipe; read the [justfile](justfile) for what each one does. The pre-commit hook runs rustfmt and clippy; skip it with `git commit --no-verify` or `REEF_SKIP_HOOKS=1` when you need to.

`just ci` also lints the installer with [ShellCheck](https://www.shellcheck.net) (`brew install shellcheck` or `apt-get install shellcheck`).

[jj](https://github.com/jj-vcs/jj) is optional. One test, `open_repo_created_by_jj_cli` in `crates/reef-lib/tests/jj_spike.rs`, uses the `jj` binary and skips itself when `jj` is not on `PATH`.

## Workflow

- Work on a branch, in a fork or in this repository if you have access. Never push to `main`.
- One ticket per pull request. A change that is not a ticket says so in the PR's Ticket section, with a sentence on why it does not need one.
- PR titles lead with the ticket ID and its name, for example `R0.04 jj-lib embedding spike`.
- Fill in the [PR template](.github/PULL_REQUEST_TEMPLATE.md).
- PRs are squash merged, so each ticket lands as one commit on `main` with the PR description as its body. Push fixes as new commits on the branch; there is no need to rewrite history during review.
- `main` is protected. Merging requires the `CI passed` check, which succeeds only when the `rustfmt`, `API schema`, `cargo deny`, `ubuntu-24.04`, `macos-15`, `overlayfs`, `synth determinism`, `CLI shape`, `web`, and `web e2e` jobs all succeed, and the DCO check. The rule is set by hand in the repository settings (Settings, Branches), not through the API.
- If a ticket brief and `docs/dev-phases.md` disagree on scope, the brief wins for the implementation; fix `dev-phases.md` in the same PR.

## Commit messages

Follow the subject style already in the log, plus the trailers and ticket line below:

- Subject in the imperative mood and sentence case, with no trailing period and no `feat:`-style prefix, under 72 characters.
- A blank line, then a body that explains why the change was made rather than restating the diff, wrapped at 72 columns.
- When a commit implements a ticket, the body's first line names it (`R0.04.`). The ID goes in the body and the PR title, not the subject.
- Every commit carries a `Signed-off-by:` trailer ([DCO](#dco)). Use `git commit -s`.
- Add a `Co-Authored-By:` trailer for each pair or agent that helped write the commit.

Examples from the history:

```text
Lay out the workspace as four crates with a fixed dependency direction
Check staged content in the hook and quote just recipe arguments
Benchmark snapshots of the synthetic repository with criterion
```

A full message, adapted from the last of those:

```text
Benchmark snapshots of the synthetic repository with criterion

R0.06.

The design's first goal, O(what changed) rather than O(repository size),
needs numbers before the daemon exists. reef-lib gets a criterion bench
that builds a reef_testing::synth tree with a jj repo under
target/bench-fixtures/ and times seven operations.

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>
Signed-off-by: Ada Contributor <ada@example.com>
```

## Code conventions

- **Errors and logging** follow [ADR 0009](docs/adr/0009-error-and-logging-conventions.md): `thiserror` enums that implement `Classify` in libraries, `anyhow` only in `main.rs` and tests, `tracing` events with field names from `reef_lib::telemetry::fields`.
- **Lints** are set once in `[workspace.lints]` in the root `Cargo.toml` (clippy pedantic plus a few stricter lints) and CI denies warnings. Every `allow` carries its reason: `reason = "..."` in the attribute in code, a trailing comment in `Cargo.toml`. `clippy.toml` relaxes `unwrap`, `expect`, `dbg!`, and printing in tests only.
- **Visibility** is `pub(crate)` by default. Make an item `pub` only when another crate needs it; the `unreachable_pub` lint catches the rest.
- **Docs** on every public item. `missing_docs` is on, and `just doc` builds with `-D warnings` and private items.
- **Tests**: unit tests sit next to the code in a `#[cfg(test)] mod tests`; integration tests go in the crate's `tests/` directory; benchmarks go in `benches/` (see [docs/benchmarks.md](docs/benchmarks.md)).
- **Crate boundaries** follow [ADR 0007](docs/adr/0007-crate-layout.md): `reef-vcs` → `reef-daemon` → `reef-lib`, and `reef-testing` depends on none of them. `reef-api` depends on no Reef crate; `reef-daemon` and `reef-vcs` depend on it, and `reef-lib` never does ([ADR 0029](docs/adr/0029-daemon-api-v1.md)).

## Changing the API

The schema in `crates/reef-api/proto/reef/api/v1` is the API of record ([ADR 0029](docs/adr/0029-daemon-api-v1.md)); v1 only grows.

1. Edit the protos: add, never remove, renumber, rename, or move. A field that should go is marked `[deprecated = true]`.
2. `buf format -w`, then `just api` (lint and format) and `just api-breaking` (against `main`; from a Reef workspace, pass the registered root's `.git`).
3. Bump `API_REVISION` in `crates/reef-api/src/lib.rs` by one, and append the digest `cargo nextest run -p reef-api` prints to `API_DIGESTS`.
4. Review the change against ADR 0029's API review checklist, and list the addition in the CHANGELOG.

## Working on the web UI

The web UI is a React and TypeScript app in `web/`, which the daemon embeds and serves on a loopback listener ([ADR 0047](docs/adr/0047-web-ui-transport-and-local-listener.md)). It needs Node at the version in `web/.node-version` (24); nothing else in the repository does.

```sh
just web-install   # npm ci in web/, then generate the TypeScript client from the protos
just web-gen       # regenerate it, for an editor's types after pulling a schema change
just web-dev       # Vite's dev server on http://127.0.0.1:5173
just web-build     # build the app into crates/reef-web/assets for the next cargo build
just web-check     # lint, typecheck, Vitest, build, then reef-web's tests against the build
just web-e2e       # the app in Chromium, Firefox and WebKit against a scratch daemon
just web-clean     # remove the build and the generated code: back to the placeholder
```

Nothing built or generated is committed: `web/src/gen/` (the client, written by `buf generate` and `protoc-gen-es` from `crates/reef-api/proto`) and `crates/reef-web/assets/` (the build) are ignored. The npm scripts that need the generated code (`build`, `typecheck`, `lint`, `test`, `dev`) regenerate it first, so it always matches the protos in the checkout; an editor shows whatever `just web-gen` last wrote.

Without a build, `reef-web` embeds a placeholder page, so a plain `cargo build`, `cargo install` and `just ci` need no Node. A daemon built that way refuses `reef ui` with `WEB_UI_NOT_BUILT`. Run `just web-build`, then `cargo build`, for a daemon that serves the app; `just web-clean` and another `cargo build` go back. Released binaries always embed the app: the release workflow builds it once and every target embeds the same bytes.

CI's `web` job runs `just web-check` on every pull request. It is where a schema change that breaks the app's generated types fails, so a pull request that changes the protos may need app changes too. The daemon's own web tests (`crates/reef-daemon/tests/web_ui.rs`) run in the `check` jobs against the placeholder.

`just web-e2e` is the browser smoke test (`web/e2e/`, Playwright), and CI's `web e2e` job runs it. Install the browsers once with `cd web && npx playwright install chromium firefox webkit`. The recipe builds the app and then `reef` and `reef-daemon` embedding it, so it never tests stale binaries; the test starts its own daemon on a scratch state directory in `TMPDIR`, with its own home and configuration, and touches no other daemon. Arguments go to Playwright: `just web-e2e --project=firefox` runs one browser, `--headed` shows it. A failure leaves a trace in `web/test-results/` for `npx playwright show-trace`, with the daemon's logs and every `reef` call attached; CI uploads the same as the `web-e2e` artifact. Waits are on the page's state, never on time: ESLint rejects `waitForTimeout` and `networkidle` in `e2e/`, and nothing is retried. A view that later tickets add gets a step in `e2e/smoke.spec.ts`.

To work on the app against a real daemon, run `reef ui --print` with a daemon built after `just web-build` (or any daemon whose `CreateWebSession` works), then start the dev server with `REEF_UI_URL` set to the printed `url`: Vite proxies `/reef.api.v1.` paths to the daemon's listener and rewrites `Host` and `Origin` to its own, so the daemon has no dev mode. Open `http://127.0.0.1:5173/#session=<token>` with the token from the printed session URL. That URL is a credential until the daemon stops; keep it out of issues and logs.

The app follows a few rules that keep it live and safe; ESLint rejects `setInterval` and `dangerouslySetInnerHTML`:

- No `setInterval`, and `setTimeout` only in `live/backoff.ts`: views update from the streams, never on a timer.
- No `dangerouslySetInnerHTML`, and no API text rendered as HTML or Markdown: repository content may be hostile, and script in the page would run with the session's token.
- State colors (`--state-<name>`) are for object states only, always with the state's shape and word.
- Every runtime dependency runs with the token, so adding one needs a reason in the pull request.

## ADRs

Write an architecture decision record for any choice of dependency, protocol, storage format, on-disk layout, or public API shape that a later contributor could reasonably ask "why" about. Implementation details inside one module do not need one. [docs/adr/README.md](docs/adr/README.md) has the format, the statuses, and the index.

To allocate a number, take the next free one after the highest number in the index (reserved rows count as taken) and claim it in your PR by adding the index row. If another PR merges the same number first, renumber yours. A ticket brief that already assigns a number wins.

## Agents

Agent-authored pull requests are welcome and expected; Reef is built for a world where agents write most of the code, and it is built that way too. State which model produced the work, with its version, in the PR's Authorship section, and add a `Co-Authored-By:` trailer to the commits. The same review bar applies to agent work as to anyone else's: CI must pass, the checklist must be honest, and the person who opens the PR is answerable for it and signs off its commits.

## DCO

Reef uses the [Developer Certificate of Origin](https://developercertificate.org/) instead of a contributor license agreement ([ADR 0010](docs/adr/0010-dco-instead-of-cla.md)). By adding a `Signed-off-by:` line to a commit you certify that you wrote it or otherwise have the right to submit it under the project's Apache 2.0 license. You keep your copyright; nothing is assigned.

Sign off with `git commit -s`, which appends:

```text
Signed-off-by: Your Name <you@example.com>
```

using `user.name` and `user.email` from your git config. Use your real name. The DCO check also requires the sign-off to match the commit's author name and email, so a mismatched `user.email` is the usual cause of a failing check. The DCO check on each PR fails if any commit lacks a sign-off. To fix it:

```sh
git commit --amend -s --no-edit       # the last commit
git rebase --signoff origin/main      # every commit on the branch
git push --force-with-lease
```

For agent-authored commits, the human who submits them signs off and thereby makes the certification on the agent's behalf.

## Release process

Not yet defined; see R4.17 in [docs/dev-phases.md](docs/dev-phases.md).
