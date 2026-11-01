# Issue #128 Playwright smoke test

Phase 2 · Size S (no row; see "Size") · Fixes: issue #128 · Depends on: R2.18 (the app, the loopback listener and its gate, `reef ui`, the shared streams), R2.19 (the smartlog and the overview's link to it) · Unblocks: none listed (R2.20, R2.21 and R3.31 add their views' checks to the same test)

## Context

R2.18 shipped the web UI with no browser test in CI (its decision 6) and recorded manual checks in #127 instead. R2.19 shipped the smartlog the same way (its decision 5) and added one check to #128. The checks each PR ran by hand, in Chrome on macOS only:

- `reef ui` opens a session, and the token is gone from the address bar;
- a `reef describe` in a terminal updates the overview's row live;
- several tabs hold one operation stream and one presence stream;
- a daemon restarted on a fixed `[ui] listen` port shows the no-session page;
- (R2.19) the overview's repository link opens `/repos/<id>/smartlog`, and a `reef describe` of a workspace's change updates that change's row in place, at the same position, with the new description, without a reload.

Nothing in CI runs the built app in a browser. The `web` job runs Vitest under jsdom, with fake services over Connect's `createRouterTransport`. The release's `install` job fetches `/` with `curl` and finds the hashed script name. Neither tests a real `fetch` stream, Web Locks, a `BroadcastChannel`, `history.replaceState`, or the daemon's gate seen from a real browser. These are the parts where browsers differ and where the app has its own logic (`live/share.ts`).

This ticket adds one Playwright test that runs the five checks against a real `reef-daemon` built with the real assets, on a scratch state directory, in Chromium, Firefox and WebKit, in a new CI job.

### What this ticket will not achieve

- **It does not test Safari.** Playwright's WebKit on Linux is the GTK port, with its own network stack. A pass there is evidence about WebKit's DOM, Web Locks and fetch streaming, not about Safari on macOS. A local run on a Mac uses Playwright's macOS WebKit, which is closer, and the PR records one such run. A macOS CI job is out of scope ("Out of scope").
- **It is a smoke test, not coverage.** One scenario, a happy path plus the restart. The views' logic stays tested in Vitest (W1 to W8, R2.19's tests 2 to 11).
- **It does not test the release's binaries.** CI builds `reef` and `reef-daemon` in debug from the same commit and the same `npm run build`. The release keeps its own proof, the grep and the `curl` (`release.yml:163` to `:172`, `:246` to `:258`).

### What was verified while writing this ticket

Reef at `68cd49a` (#142 merged). `@playwright/test` 1.64.1 is the latest release (2026-10-14, `npm view`); it depends only on `playwright` 1.64.1, which depends on `playwright-core` 1.64.1, all Apache-2.0, `engines.node >= 20`. Paths are under `crates/` unless they start with `docs/`, `web/`, `scripts/` or `.github/`.

- **The `web` CI job** (`.github/workflows/ci.yml:227` to `:260`) runs on `ubuntu-24.04` with a 15-minute timeout. It sets up Node from `web/.node-version` (24), with the npm cache on `web/package-lock.json` (`:239` to `:243`), plus the Rust toolchain, nextest and just. It runs `npm ci` (`:251` to `:253`), then `just web-check` (`:254`), then `npm audit --omit=dev`, allowed to fail (`:257` to `:260`). It builds no daemon: `just web-check` (`justfile:71` to `:73`) is lint, typecheck, Vitest and `npm run build`, then `REEF_WEB_EXPECT_BUILT=1 cargo nextest run -p reef-web`, a crate with no dependencies. In the two latest runs on `main` (41261158457 at `4a89a16`, 41245418454 at `ce8c67c`), the whole job took 55 s and 54 s, and its Check step 19 s and 22 s. R2.18's estimate was 3 to 4 minutes.
- **The critical path is elsewhere.** In the same runs, `ubuntu-24.04` took 34 and 25 minutes, `macos-15` 37 and 35 minutes, and `overlayfs` 30 and 29 minutes. `ci-ok` (`ci.yml:300` to `:323`) needs every job (`:305`), passes each result through `env` (`:311` to `:320`) and requires all to be `success` (`:323`). So the wall time of CI is the macOS job's, about 35 to 37 minutes. The macOS job's cap is 50 minutes (`:95`), and a cold build adds about 8 minutes (`:90` to `:94`). A Linux job that finishes within about 25 minutes adds nothing to it.
- **Which jobs build the binaries.** `check` builds `--workspace --all-targets` in debug (`just build`, `ci.yml:121`, 1.5 to 2 minutes warm on Linux) and runs `scripts/test-remote.sh target/debug/reef target/debug/reef-daemon` on Linux (`:135` to `:137`). Those binaries embed the placeholder: the `check` jobs stay Node-free by R2.18's decision 1. `overlayfs` builds the privileged tests and `reef` (`:184` to `:187`). No job has both Node and a daemon build.
- **The release.** Its `web` job (`release.yml:79` to `:110`) builds the assets once, with no cache on a real release, and uploads `web-assets`. `build` downloads them (`:143` to `:146`), and its smoke test greps each binary for the hashed script name and against the placeholder's sentence (`:163` to `:172`). `install` on `ubuntu-24.04` runs the installed `reef ui --print` and fetches `/` (`:239` to `:258`). The release has no browser.
- **The web toolchain.** `web/package.json` pins every version exactly: TypeScript 6.0.3 (`:47`), because typescript-eslint 8.72.1 (`:48`) supports below 6.1 (R2.18's "Departures in the implementation"); Vite 8.3.5, Vitest 5.0.5, ESLint 10.14.0 and Prettier 3.9.10. Every script that needs `src/gen` regenerates it first (`:11` to `:24`). Vitest runs `src/**/*.test.{ts,tsx}` under jsdom (`vite.config.ts:42` to `:46`). `tsconfig.json` includes `src` and `vite.config.ts` only (`:19`), with `types: ["vite/client", "node"]`. ESLint ignores `src/gen/` and `node_modules/` (`eslint.config.js:18`), type-checks through `projectService` (`:24`), bans `setInterval` everywhere and `setTimeout` outside `live/backoff.ts` (`:30` to `:52`), and gives Node globals to `scripts/**`, `vite.config.ts` and `eslint.config.js` (`:61` to `:63`). Prettier checks `.` minus `src/gen/`, `node_modules/` and the lockfile (`.prettierignore`). `scripts/licenses.mjs` lists only packages that are neither `dev` nor `optional` in the lockfile (`:25` to `:28`), so a dev dependency never reaches `THIRD_PARTY_LICENSES.txt`.
- **The bundle budget.** `crates/reef-web/tests/assets.rs:123` to `:143`: at most 600 KiB of assets and 450 KiB of JavaScript, run by `just web-check` against the real build.
- **Dependabot** (`.github/dependabot.yml`) covers `github-actions` and `cargo`, not npm. No npm dependency gets update PRs today.
- **`reef ui --print`.** The first `CreateWebSession` binds `[ui] listen` (`reef-daemon/src/web/mod.rs:186` to `:192`); a daemon that never mints a session opens no TCP port. With stdout piped, the CLI prints `reef_client::json::WebSession` (`reef-client/src/json.rs:606` to `:616`): `url` (`http://127.0.0.1:<port>/`), `launch_path` (null with `--print`), `opened: false`, and `session_url` (`url` plus `#session=` and 43 base64url characters) (`reef-vcs/tests/vcs.rs:2567` to `:2574`). A placeholder build refuses with `WEB_UI_NOT_BUILT`, exit 5.
- **The token in the browser.** `session.ts` reads `#session=<token>` (`:9`), stores it in `localStorage` under `reef.session` (`:7`), and replaces the URL with `pathname + search` (`:41` to `:53`). `App.tsx` renders `NoSession` with no token, or once the daemon refuses it (`:135`). On loopback, the page's text is "This page has no session with the daemon. Run `reef ui` in a terminal." (`views/NoSession.tsx:16` to `:21`).
- **The restart.** `[ui] listen` is a loopback IP literal and a port, default `127.0.0.1:0` (`reef-daemon/src/config.rs:676` to `:696`). Sessions are never persisted, so a restart ends them all (ADR 0047, R2.18's decision 7). A restarted daemon binds nothing until the next `CreateWebSession`. Until a `reef ui` runs, an open tab's retries are refused at the TCP level and the tab shows "Reconnecting". After it runs, the tab's next retry reaches the new daemon with a dead token, gets `WEB_SESSION_INVALID` (advice at `web/mod.rs:138` to `:141`), and the tab renders the no-session page. The bind is `tokio::net::TcpListener::bind` (`web/mod.rs:269`), which is mio's, and mio sets `SO_REUSEADDR` on Unix (mio 1.2.4, `src/net/tcp/listener.rs:63` and `:82`). The old daemon's connections in `TIME_WAIT` therefore do not block the port on the same address.
- **The gate seen from a test.** A request with no `Origin` and no `Sec-Fetch-Site` is judged by its token alone (R2.18, gate step 5). Assets are public. So Node's `fetch(url)` of `/` works with no session, as the release's `curl` does.
- **Counting streams.** The stream layer leads from the tab holding the Web Lock `reef-live` and republishes on the `BroadcastChannel` `reef-live` (`web/src/live/share.ts:6` to `:13`, `:24`, `:104` to `:125`). Without `navigator.locks` or `BroadcastChannel`, every tab leads alone (`:106` to `:109`). The two streams are `POST /reef.api.v1.OperationService/SubscribeOperations` and `POST /reef.api.v1.PresenceService/SubscribePresence` over gRPC-web, binary, to the page's origin (`api/connection.ts:60` to `:67`). Over plain `http`, browsers speak HTTP/1.1 to the listener, so each open stream holds one TCP connection, and unary calls use other connections that stay open, idle, in the browser's pool. The daemon counts open subscriptions in `Subscriptions::active()` (`reef-daemon/src/subscribe/mod.rs:105` to `:109`; admitted at `service/operations.rs:541` to `:544` and `service/presence.rs:251` to `:254`). It does not expose the count: `GetDaemonStatusResponse` has no such field (`daemon.proto:64` to `:91`). It logs no line per subscription either: the web module logs `web UI listening` (`web/mod.rs:293`), and the broker's summary counts topics, not subscribers (`subscribe/broker.rs:323`). R2.18's brief said its manual check read "the daemon's log"; #127 in fact counted connections ("three tabs hold two daemon connections").
- **The overview's DOM.** `<h2>Repositories</h2>` and one `<table>` (`views/Overview.tsx:105`, `:113` to `:135`). A row is the root's last path component as a `Link` to `smartlogHref({ repo: id })`, with the full root in `title`, then the id's first 8 characters, then the latest operation as `<kind> · <agent> · <workspace 8> · <time>`, with the operation's description in `title`, then the number of active agents (`:22` to `:66`). `smartlogHref` is `/repos/<encoded id>/smartlog` (`views/Smartlog.tsx:63` to `:69`). The header's connection indicator is a `span` with `aria-live="polite"`, the word `Connecting`, `Live`, `Reconnecting` or `Session ended` (`ui/ConnectionIndicator.tsx:6` to `:11`, `:31`), inside the page's `<header>`.
- **The smartlog's DOM.** The rows are a `<ul role="listbox" aria-label="Changes">` (`Smartlog.tsx:488` to `:493`), one `<li role="option">` per commit, keyed by commit id (`:507`), with `aria-label` from `accessibleName` (`:99` to `:114`, `:147`), `data-commit` (`:150`) and a `title` naming the row's cause, `"<state>; changed at <time> by <agent>, <kind>"` (`:139` to `:141`, `:157`). The accessible name is comma-separated: the state's label, `change <8 of reverse hex>`, then bookmarks, tags, `workspaces <8 of each id>`, `agents <names>` when presence names the change, `empty`, the description's first line or `no description`, and the edge notes. `shortId` is the first 8 characters (`ui/ids.ts:30` to `:32`). A rewrite is a new commit, so a new `<li>`, placed at the old one's position by R2.19's ranks ("Order").
- **The scratch daemon pattern** is `scripts/test-remote.sh`. It makes a `mktemp -d` directory and removes it in an `EXIT` trap (`:25` to `:35`). It writes `config.toml` into the state directory before the daemon starts (`:48` to `:52`). It runs `reef-daemon --foreground` with `REEF_STATE_DIR`, `REEF_SOCKET` and `HOME` (`:63` to `:67`), polls `reef -o json daemon status` every 100 ms for up to 10 s (`:74` to `:79`), and registers a `git init` fixture (`:55` to `:57`, `:83`). Then it runs `ws create --repo <id> --purpose ...` and `describe --workspace <ws> -m ...` (`:115`, `:118`), with `--no-autostart` on the client (`:97`). The release's `install` job uses `--state-dir` and stops with `reef daemon stop` (`release.yml:254`, `:258`).
- **Environment and paths.** The client and daemon read `REEF_STATE_DIR`, `REEF_SOCKET` and `REEF_CONFIG` (`reef-client/src/paths.rs:35` to `:39`). A socket path may be at most 103 bytes (`:44`), macOS's `sun_path`. A non-default state directory keeps its socket in itself by default. A config file named by `REEF_CONFIG` must exist (`PathsError::Missing`). There is no `REEF_DAEMON_BIN`. `reef` autostarts the `reef-daemon` beside its own executable, else one on `PATH` (`reef-vcs/src/cli/autostart.rs:117` to `:139`), with its stderr sent to `/dev/null` (`:148` to `:160`). `--agent` is a global flag with `env = REEF_AGENT` (`reef-vcs/src/cli/mod.rs:144`). `reef ws run` exports `REEF_WORKSPACE`, `REEF_WORKSPACE_PATH`, `REEF_REPO` and `REEF_SOCKET` (`cli/env.rs:10` to `:28`), so a developer running the test from a Reef workspace has them set. The daemon logs to stderr and to `<state>/logs/daemon.log.<date>`, filtered by `REEF_LOG` (`reef-daemon/src/logging.rs:5` to `:10`). Its tests check that no session token reaches the logs (`tests/web_ui.rs`, `tokens_never_reach_logs`).
- **The JSON the harness reads**: `repo register` prints the repository with `id` and `root` (`reef-vcs/tests/cli.rs:272` to `:276`). `ws create` prints the workspace with `id` and `change_id_reverse_hex` under `-o json` (`reef-client/src/json.rs:282`). Brief output, the piped default, omits the change id, so the harness sets `REEF_OUTPUT=json`.
- **Leases and presence.** A workspace's lease defaults to 30 minutes (`config.rs:266` to `:269`), longer than the test runs. The daemon's `LEASE` presence entries lapse after two missed heartbeats (R2.10). Heartbeats are every 60 s, and the test sends none, so a row's `agents ...` label may disappear during a run. The test never asserts on it.
- **The watcher.** Nothing in the scenario writes a file in a workspace, so no snapshot records an operation after the workspaces are created. The daemon's own `web_ui::streams_over_grpc_web` sets `watcher.debounce` to an hour for the same reason. The test keeps the default configuration and relies on writing nothing.
- **Conventions.** `CONTRIBUTING.md:36` names the jobs behind `CI passed`, and the list is already stale: it omits `overlayfs` and `CLI shape`. "Working on the web UI" (`:91` to `:115`) lists the `just web-*` recipes. The CHANGELOG lists test and benchmark infrastructure under Added (the subscription load test, the onboarding benchmark).

## Decisions

### Size

S. It is one Playwright test file, its harness, a config, lint and type settings, a `just` recipe, one CI job and documentation. There is no Rust, no API change and no app code. The uncertainty is CI time and cross-browser behavior, and the PR measures both.

### Where it runs: a new `web e2e` job beside `web`

The issue says "in the `web` job". This brief gives the test its own job, `web-e2e` (name `web e2e`), on `ubuntu-24.04`, with no `needs`, so it runs in parallel with everything else, and adds it to `ci-ok`. Open question 1 asks the user to confirm.

The test needs `reef` and `reef-daemon` built with the real assets. Four places were weighed:

- **Steps in `web`.** Simplest: `just web-check` has just written `crates/reef-web/assets`, so a `cargo build -p reef-vcs -p reef-daemon` after it embeds them, with no artifact. The cost is a 55-second job becoming a 6 to 8-minute one warm and about 15 cold, past its 15-minute cap. Lint, typecheck and Vitest, the job's common failures, then report no sooner, but the job's one green tick covers both. Its Rust cache, today only `reef-web`, would grow to the daemon's whole debug dependency tree.
- **A new job (chosen).** `web` keeps its one-minute feedback and its tiny cache. The browser job gets its own cache (rust-cache keys by job), its own timeout, its own check name, and a re-run that repeats only it. That matters if it flakes, given the repository's known timing flakes under load. It builds the app itself (`npm ci` and `npm run build`, about 25 s) rather than waiting for `web`'s artifact, so neither waits for the other. It is off the critical path either way. At 6 to 15 minutes against the macOS job's 35 to 37, it changes CI's wall time by nothing.
- **`check` on Linux, reusing its binaries.** It already builds both binaries, but with the placeholder. The real assets would put Node in `check`, which R2.18's decision 1 keeps Node-free. They would also make `check` wait on `web` and change what `check`'s daemon tests embed. Rejected.
- **A job depending on `check`'s artifacts.** The artifacts embed the placeholder, so the job would still rebuild `reef-web` and relink `reef-daemon`, which needs the daemon's dependency tree compiled anyway. It would also start only after `check`'s 25 to 34 minutes, which puts it on the critical path. Rejected.

No path filter: a daemon change can break the app as easily as a `web/` change, and a skipped required check counts as passing (the comment at `ci.yml:302`).

The release workflow is not changed. Its `web` job builds the assets, every target embeds them, and the `install` job's `curl` proves a released daemon serves them. Putting browsers in the release would make release day depend on Playwright's browser CDN as well as npm.

### Browsers: Chromium, Firefox and WebKit, all in CI

"Firefox and WebKit can follow if the job's time allows." It does. The job is off the critical path by about 20 minutes, and each browser adds about a minute of test and under a minute of download and system packages. The two other browsers test what this test exists for: Web Locks, `BroadcastChannel`, streamed `fetch` bodies, and `localStorage` shared across a context's pages. Those are implemented separately in Gecko and WebKit. R2.18 and R2.19 listed Firefox and Safari among the manual checks and never ran them.

The three are Playwright projects, `chromium` (`devices["Desktop Chrome"]`), `firefox` (`devices["Desktop Firefox"]`) and `webkit` (`devices["Desktop Safari"]`), run one after another (`workers: 1`) in one `playwright test`. Each project gets its own scratch daemon, so a failure in one cannot leak into the next.

A failure in one browser is a finding, not noise. If a browser fails a step for a reason in the app (say, the second tab opens its own streams), the PR files an issue and marks that step `test.fixme` for that project only, naming the issue. If it fails for a reason in Playwright's port (a WebKit GTK quirk with no Safari counterpart, shown by a passing local macOS WebKit run), the PR drops that project from CI, keeps it for local runs, and files an issue with the evidence. Either way the PR says which, and the user decides before merge (open question 2).

### Toolchain

- **`@playwright/test` 1.64.1**, exact, as a dev dependency (`npm install -D -E @playwright/test@1.64.1`). It is the latest stable release and is 17 days old. Installing it downloads no browser. Its `.d.ts` files compile under TypeScript 6.0.3 with `skipLibCheck`, as every other dependency's do. Playwright transpiles its test files itself and needs no `tsc` step.
- **Browsers in CI**: `npx playwright install --with-deps --only-shell chromium firefox webkit`, after `npm ci`. `--only-shell` installs Chromium's headless shell, which is all a headless run launches; check the flag against 1.64.1's `install --help` before relying on it. `--with-deps` installs the system packages with `sudo apt-get`, which the runner allows. Browsers are not cached, following Playwright's CI guidance: restoring them takes about as long as downloading, and the system packages must be installed on every run anyway. The PR records the step's time. If it exceeds 2 minutes, cache `~/.cache/ms-playwright` keyed on the Playwright version and run `npx playwright install-deps` alone.
- **Chromium's sandbox**: Playwright launches Chromium without its sandbox by default (`chromiumSandbox: false`), so Ubuntu 24.04's AppArmor restriction on unprivileged user namespaces does not apply.
- **No `webServer`**: Playwright's `webServer` starts one server for the whole run and cannot restart it mid-test. The harness owns the daemon instead.

### Where the test lives, and how it stays out of Vitest, the bundle and the lint's blind spots

- `web/e2e/smoke.spec.ts`, the test; `web/e2e/reef.ts`, the harness; and `web/playwright.config.ts`. Playwright's `testDir` is `e2e`, so it never picks up Vitest's `src/**/*.test.tsx`, which need jsdom. Vitest's `include` is `src/**` (`vite.config.ts:44`), so it never picks up `e2e/`. The suffixes differ (`.spec.ts` against `.test.ts`), so a reader can tell the two apart by name.
- **The bundle**: nothing in `src/` imports `e2e/` or `@playwright/test`, and Vite builds from `index.html`'s imports. So the built assets are byte-identical to `main`'s, with the same hashed script name. The PR checks that, and `bundle_budget` keeps holding the line. `licenses.mjs` skips dev dependencies, so `THIRD_PARTY_LICENSES.txt` is unchanged.
- **Types**: `tsconfig.json`'s `include` gains `"e2e"` and `"playwright.config.ts"`, so `npm run typecheck` and `npm run build`'s `tsc --noEmit`, which `web` already runs, type-check the test with no browser. One tsconfig keeps `projectService` simple. The DOM lib in that config is what `page.evaluate` callbacks need anyway.
- **ESLint**: one block for `e2e/**` and `playwright.config.ts`. It gives them Node globals, turns off `@typescript-eslint/no-non-null-assertion` as the test block does, and sets `no-restricted-syntax` to ban `CallExpression[callee.property.name='waitForTimeout']` (no sleeps) and the literal `'networkidle'` (the streams never let the network go idle, so a wait on it never ends). The block restates the rule's whole list, because a later block's options replace an earlier block's, they do not merge. The `setTimeout` and `setInterval` bans stay. The harness waits through Playwright's `expect.poll` and child-process events, and never needs a timer.
- **Ignores**: `test-results/`, `playwright-report/` and `blob-report/` go into ESLint's `ignores` (the HTML report ships JavaScript that `eslint .` would otherwise lint), into `.prettierignore`, and into the root `.gitignore` as `web/test-results/`, `web/playwright-report/` and `web/blob-report/`.
- **`package.json`** gains `"e2e": "playwright test"`, with no `pree2e`. The `just` recipe builds first.

### The harness: one scratch daemon per browser

`web/e2e/reef.ts` exports a Playwright fixture, `reef`, scoped to the test. It follows `scripts/test-remote.sh`'s pattern, in TypeScript:

1. **Binaries.** It uses `REEF_E2E_BIN_DIR`, defaulting to `../target/debug` resolved from `web/` (the recipe passes `${CARGO_TARGET_DIR:-target}/debug`, absolute). If `reef` or `reef-daemon` is missing there, it fails at once with "run `just web-e2e`, which builds the app and both binaries".
2. **A scratch root**: `realpath(mkdtemp(join(os.tmpdir(), "reef-e2e-")))`. Under it go `state/` (mode `0700`), `home/`, the fixture repository `e2e-fixture/`, and the socket `sock`. The socket's path is asserted to be at most 103 bytes before anything starts, with a message naming `TMPDIR`. On a Mac, `/private/var/folders/.../T/reef-e2e-XXXXXX/sock` is about 70 bytes; under `web/test-results/` it would not fit, which is why the root is not there.
3. **One environment for every child**: `process.env` without every `REEF_*` variable and without `RUST_LOG`, plus `REEF_STATE_DIR`, `REEF_SOCKET`, `REEF_CONFIG=<state>/config.toml`, `REEF_OUTPUT=json`, `REEF_LOG=info`, `NO_COLOR=1`, and `HOME=<root>/home`. The root's own home works as `test-remote.sh`'s `HOME=/tmp` does: no user configuration (jj's, git's) reaches the daemon or the fixture. Git calls also get `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1` and fixed author and committer names and emails, so a developer's commit signing or hooks cannot break the fixture. This is what makes the test safe to run from inside `reef ws run`.
4. **The configuration, before any `reef` call**: `<state>/config.toml` with `[ui]` and `listen = "127.0.0.1:0"`. The first daemon uses the default, kernel-chosen port, which is what users get. `writeConfig({ listen })` rewrites the file before a restart.
5. **The fixture**: `git init -q -b main e2e-fixture`, one file, one commit, and `reef --no-autostart repo register <path>`. That takes `id` and adds jj beside git, as a user's first command does. Then two workspaces, `wsA` and `wsB` in that order, with `reef --no-autostart ws create --repo <id> --purpose "e2e smoke"`, each giving `id` and `change_id_reverse_hex`. All of this is done before any browser opens, so its operations are recorded before the page's first read. Two workspaces are needed so that "same position" can fail (see the scenario's step 6). jj lists the most recently rewritten commit first, so a describe of the only workspace, or of the newest one, would leave it on top in jj's order too. The test would then pass even with R2.19's stable order removed.
6. **The daemon**: `spawn(<bin>/reef-daemon, ["--foreground"], { env, cwd: root })`. Stdout and stderr go to `<root>/daemon-<n>.log`, where `n` counts the starts. Readiness is `expect.poll(() => reef("daemon", "status") succeeds, { timeout: 30_000, intervals: [100] })`. A child that exits early fails the poll at once, with its log. The harness spawns the daemon itself rather than letting `reef` autostart one: autostart discards the daemon's stderr, and the harness needs the pid, the log and a controlled restart.
7. **`reef(...args, { agent })`**: `execFile(<bin>/reef, ["--no-autostart", ...args])`, with `REEF_AGENT` set when `agent` is given. It parses stdout as JSON and throws with exit code, stdout and stderr on failure; stderr carries the error's `hint` and `next`. `--no-autostart` everywhere means a call made while the daemon is down fails instead of starting a second, unlogged daemon. That would break the restart check.
8. **`session()`**: `reef("ui", "--print")` returns `{ url, sessionUrl, token }`. Exit 5 with `WEB_UI_NOT_BUILT` is reported as "this reef-daemon embeds the placeholder; run `just web-e2e`".
9. **`stop()`**: `reef("daemon", "stop")`, then the child's `exit` event within 30 s, else `SIGKILL` and a failure. `start()` runs step 6 again on the same state directory.
10. **Teardown**, always: stop the daemon if it runs. If the test failed, attach to the test result each `daemon-<n>.log`, `<state>/logs/daemon.log.*`, `reef op log --repo <id> --limit 20`, and the transcript of every `reef` call (arguments, exit code, stdout, stderr). Every `#session=...` in the transcript is replaced with `#session=<redacted>`. Then remove the root.

Each project's test gets its own fixture, so three browsers make three daemons, one at a time.

### The scenario

One test, `smoke`, per project, in `test.step`s that share the daemon and the tabs, in this order. Each step's name is what a trace shows.

1. **The daemon serves this checkout's app.** `session()` gives the URL. Node's `fetch(url)` gets `index.html`, and its `assets/index-<hash>.js` equals the one in `crates/reef-web/assets/index.html`. A mismatch fails with "the daemon embeds another build: run `just web-e2e`". That catches binaries left from another checkout or a stale target directory, a known trap in Reef workspaces.
2. **The token leaves the address bar.** `page.goto(sessionUrl)`. Then:
   - `expect(page).toHaveURL(url)`, exactly, with no fragment;
   - `page.url()` does not contain the token;
   - `localStorage.getItem("reef.session")` equals the token, so it was stored, not dropped;
   - the header's indicator reads `Live`;
   - the heading `Repositories` is visible.
3. **The overview lists the repository.** A row holds a link named `e2e-fixture` and the text of the repository id's first 8 characters.
4. **A second tab shares the streams.** `context.newPage()`, then `goto(url)` with no fragment. Its token comes from storage. It reaches `Live` and shows the same row. Then the stream counts hold ("Counting streams"): one operation stream and one presence stream in the whole context, both opened by the first tab, none by the second, both still open.
5. **A describe updates the overview, live, in both tabs.** Each page gets a marker, `window.__reefE2e = <random>`. `reef("describe", "--workspace", wsB, "-m", "e2e: overview", { agent: "e2e-overview" })`. That makes `wsB`'s change the newest commit, ahead of `wsA`'s. In both pages, the repository's row contains `describe · e2e-overview`. Both markers survive, so neither page reloaded, and the stream counts still hold. The second tab's update can only have come through the `BroadcastChannel`, so this proves the sharing delivers, not just that it opens nothing.
6. **The smartlog updates the change's row in place.** In the first page, click the link `e2e-fixture`. Then:
   - the URL is `url` plus `repos/<id>/smartlog`, and the marker survives (the router navigated, the page did not reload);
   - in the listbox `Changes`, find the option whose accessible name contains `change <c8>`, where `c8` is the first 8 characters of `wsA`'s `change_id_reverse_hex`;
   - record its index, its `data-commit`, the number of options, and the ordered list of every option's `change ` token (the order of changes);
   - assert, as a precondition, that `wsA`'s index is not 0. The view's first load follows jj's order, so `wsB`'s change, rewritten in step 5, comes first. After this step's describe, jj's order would put `wsA` first, so only R2.19's ranks keep it where it was. Without this precondition the check could pass vacuously;
   - `reef("describe", "--workspace", wsA, "-m", "e2e: smartlog", { agent: "e2e-smartlog" })`;
   - then, polled until true: the option at the same index has an accessible name matching `change <c8>, .*e2e: smartlog`, its `data-commit` differs from the recorded one (a new commit, not the old row repainted), its `title` matches `by e2e-smartlog, describe` (the cause, R2.19's "Live"), the number of options and the order of changes are unchanged, and the marker survives. The stream counts still hold, since the smartlog adds no stream (R2.19).
7. **A restart on a fixed port shows the no-session page.**
   - Take `port` from `url`, `stop()` the daemon, and see the indicator in both pages read `Reconnecting`.
   - `writeConfig({ listen: "127.0.0.1:<port>" })`, then `start()`. This exercises a configured fixed port, while the first daemon exercised the default.
   - `session()` again: its `url` equals the old `url`, so the tabs' origin reaches the new daemon, and its listener is now bound.
   - Within 30 s, both pages show "This page has no session with the daemon.", and `reef.session` is gone from storage.
   - Recovery: `page.goto(newSessionUrl)` reaches `Live` and lists the repository again. That is what the page tells a user to do.

Everything the scenario asserts is something the test controls: ids it created, descriptions and agent names it chose, and the order it recorded. It never asserts on times, the `agents ...` labels (presence may lapse, see "Verified"), the number of active agents, or another row's exact text.

### Counting streams

The check is made in the browser, through Playwright's network events, not on the daemon:

- Before the first page opens, the test listens on `context.on("request")`, which sees every request from every page of the context. It records each request whose URL path is `/reef.api.v1.OperationService/SubscribeOperations` or `/reef.api.v1.PresenceService/SubscribePresence`, with the page that sent it (`request.frame().page()`). On `requestfinished` and `requestfailed` it marks the request ended.
- **"Hold"** means, when asserted (steps 4 to 6): exactly one request per path, both from the first page, neither ended. A gRPC-web server stream is one `POST` whose body never finishes while it runs, so "one request, not ended" is "one stream, open". A resubscription would add a second request, and a stream the daemon closed would end the first, so a reconnect between steps fails the check too.

Two alternatives were rejected:

- **TCP connections** (`lsof -p <pid>` or `ss -tnp`), as #127 counted. Over HTTP/1.1 each unary call borrows a pooled connection, which then stays open and idle for a browser-defined time. A second tab's unary calls (`GetDaemonStatus`, `ListRepositories`, `ListOperations`) therefore add connections that are not streams. The count depends on the browser's pool policy and on timing, and the tools differ between Linux and macOS.
- **The daemon's own count**, `Subscriptions::active()`, is the most direct signal, but it is reachable only from Rust tests. Exposing it (say `GetDaemonStatusResponse.open_subscriptions`) is an API revision, made for a test. The browser-side check is exact for what the issue asks, so this ticket does not add one (open question 3).

### Selectors

Roles and accessible names first, with no CSS classes:

| What | Locator |
| --- | --- |
| Connection state | `page.getByRole("banner").getByText("Live", { exact: true })`, and likewise `Reconnecting`; the `<header>` is the banner landmark |
| Overview heading | `getByRole("heading", { name: "Repositories" })` |
| The repository's row | `getByRole("row").filter({ has: page.getByRole("link", { name: "e2e-fixture" }) })` |
| The smartlog link | `getByRole("link", { name: "e2e-fixture" })` |
| The rows of the smartlog | `getByRole("listbox", { name: "Changes" }).getByRole("option")` |
| A change's row | an option whose accessible name contains `change <c8>`, found by index from the options' names |
| No-session page | `getByText("This page has no session with the daemon.")` |

Two attributes are read rather than located: `data-commit`, to prove a row is a new commit, and `title`, for the row's cause. Both are in R2.19's markup on purpose (`Smartlog.tsx:150`, `:157`). The test needs no app change: every element it touches already has a role, a name or one of those attributes. If an implementer finds one missing, the fix is an accessible name in the app, not a test id.

### Waiting, timeouts and flakes

The repository's timing tests flake under load (leases, snapshots, `ws run`). This test is written so that load slows it down without failing it:

- **Waits are on state, never on time.** Every assertion is a web-first `expect` (`toHaveURL`, `toBeVisible`, `toContainText`, `toHaveAccessibleName`, `toHaveAttribute`), or `expect.poll` for values read with `evaluate` or from the network log. Lint bans `waitForTimeout` and `networkidle`.
- **Timeouts**: `expect` 10 s by default; the restart step 30 s; the test 180 s; and in CI, `globalTimeout` 15 minutes. The live paths are fast: a stream delivers within 100 ms (R2.09), the overview applies a frame, and the smartlog refreshes at most 250 ms after the operation. Ten seconds is 40 times that, for a debug daemon on a shared runner.
- **One daemon at a time.** `workers: 1` and `fullyParallel: false`, so three browsers never compete for the runner's CPU.
- **No retries**: `retries: 0`, locally and in CI. The repository retries no test (nextest has no retries configured). A pass on retry would hide exactly the cross-tab races this test exists to find, and Playwright still exits 0 on a flaky pass. If the job flakes on `main`, the cause is found and fixed, after comparing with `main` as for any flaky test. Open question 4.
- **`forbidOnly`** in CI, so a stray `test.only` fails the job.
- **Nothing races the fixture.** The workspaces are created before the browser opens, and the scenario writes no file, so no watcher snapshot can record an operation between a describe and its assertion ("Verified"). The overview's cell therefore stays on the describe. The smartlog's assertion depends only on the change's description and on the order the test recorded.

### Failure artifacts

- `use: { trace: "retain-on-failure", screenshot: "only-on-failure", video: "off" }`. A trace holds the DOM at each step, the network log and the console.
- Reporters: `list` locally; `github`, `list` and `html` (`open: "never"`) in CI.
- The harness attaches its logs on failure ("The harness", item 10), so they are in the trace viewer and the HTML report.
- The job uploads `web/test-results/` and `web/playwright-report/` as `web-e2e` with `if: failure()`, `if-no-files-found: ignore`, and 7 days' retention, as `synth-digest` keeps its artifacts.
- **Tokens in artifacts.** A trace records the session URL, the `authorization` header and `localStorage`. The token is per daemon process, the daemon is stopped before the upload, and the runner is gone after the job, so what is uploaded is a dead credential for a port on a machine that no longer exists. That is the threat model's own argument for not persisting tokens (ADR 0047, "Why per process"). The transcript and the logs carry none (redaction, and `tokens_never_reach_logs`). No threat-model row is needed, and the PR says so in one sentence.

### Local runs: `just web-e2e`

```just
# The web UI in Chromium, Firefox and WebKit against a scratch daemon (#128): builds the app,
# then reef and reef-daemon embedding it, then runs Playwright. Once: cd web && npx playwright install
web-e2e *ARGS:
    cd web && npm run build
    cargo build -p reef-vcs -p reef-daemon
    cd web && REEF_E2E_BIN_DIR="$(cd .. && pwd)/${CARGO_TARGET_DIR:-target}/debug" npx playwright test "$@"
```

(`CARGO_TARGET_DIR` may be absolute; the implementation resolves it either way.) It assumes `just web-install` has run, as `web-check` does. It always rebuilds, so a run never tests stale binaries, and step 1 checks the build anyway. `just web-e2e --project=firefox` runs one browser, and `--headed` and `--debug` pass through. `just ci` is unchanged and needs no Node or browser. On a Mac the WebKit project runs Playwright's macOS WebKit. Workspace creation can take a few seconds there in a debug build (FSEvents), within the timeouts.

### The CI job

```yaml
  # The web UI in real browsers (#128): reef and reef-daemon built with the app, a scratch
  # daemon, and Playwright in Chromium, Firefox and WebKit. Its own job so `web` stays fast,
  # off the critical path (the macOS check job is).
  web-e2e:
    name: web e2e
    runs-on: ubuntu-24.04
    timeout-minutes: 30
    steps:
      - checkout (persist-credentials: false)
      - setup-node@v6 (node-version-file web/.node-version, cache npm on web/package-lock.json)
      - setup-rust-toolchain@v2
      - setup-just@v4
      - name: Install
        working-directory: web
        run: npm ci
      - name: Browsers
        working-directory: web
        run: npx playwright install --with-deps --only-shell chromium firefox webkit
      - name: Smoke test
        run: just web-e2e
      - upload-artifact@v7, if: failure(), name web-e2e, web/test-results and web/playwright-report, 7 days
```

`ci-ok` gains `web-e2e` in `needs`, `WEB_E2E: ${{ needs.web-e2e.result }}` in `env`, and `"$WEB_E2E" == "success"` in its test. The expected time is 6 to 8 minutes warm and about 15 cold (a cold debug build of the daemon is about 8 minutes, from `check`'s comment). The PR records the first cold run and a warm one, step by step. The 30-minute cap leaves room for a cold build on a slow runner, while still ending before the macOS job does.

## Deliverables

### `web/`

- `package.json` and `package-lock.json`: `@playwright/test` 1.64.1 (exact, dev), and the `e2e` script.
- `playwright.config.ts` (new): `testDir: "e2e"`, the three projects, `workers: 1`, `fullyParallel: false`, `retries: 0`, `forbidOnly: !!process.env.CI`, the timeouts, `use` and the reporters above. No `baseURL` (the port is the daemon's) and no `webServer`.
- `e2e/reef.ts` (new): the harness and its `reef` fixture, as above. A header comment states the environment rules and why the root is in `TMPDIR`.
- `e2e/smoke.spec.ts` (new): the scenario and the stream counter.
- `tsconfig.json`: `include` gains `e2e` and `playwright.config.ts`.
- `eslint.config.js`: the `e2e/**` block and the ignores. `.prettierignore`: the three output directories.

### Repository root

- `justfile`: `web-e2e`, after `web-check`.
- `.gitignore`: `web/test-results/`, `web/playwright-report/`, `web/blob-report/`.
- `.github/workflows/ci.yml`: the `web-e2e` job after `web`, and `ci-ok`'s three additions. The comment above `web` (`:227` to `:230`) gains one sentence: the browser test is `web-e2e`.

### Documentation

- `CONTRIBUTING.md`:
  - `:36` names every job behind `CI passed`, as `ci-ok`'s `needs` lists them: `rustfmt`, `API schema`, `cargo deny`, `ubuntu-24.04`, `macos-15`, the `overlayfs` job, `synth determinism`, `CLI shape`, `web` and `web e2e`. The two that are missing today are fixed in passing.
  - "Working on the web UI" gains `just web-e2e` in the recipe block. It also gains a paragraph: install the browsers once with `cd web && npx playwright install chromium firefox webkit`; the test builds the app and both binaries, starts its own daemon on a scratch state directory, and touches nothing else; `--project` picks a browser; a failure leaves a trace in `web/test-results/` for `npx playwright show-trace`, and CI uploads the same as the `web-e2e` artifact.
- `CHANGELOG.md`, Unreleased, Added: "A browser smoke test of the web UI (`just web-e2e`, CI's `web e2e` job): a daemon built with the app, in Chromium, Firefox and WebKit, checking the session hand-off, the shared streams, live updates of the overview and the smartlog, and the no-session page after a restart."
- `docs/dev-tickets/phase-2/README.md`: a `#128` row, `issue-128-playwright-smoke-test.md`, size S, depends on R2.18 and R2.19, unblocks "none listed". No departure entry, since the issue has no row in `dev-phases.md`, and `dev-phases.md` is unchanged.
- No ADR: the decisions concern one test and its job, recorded here and in the job's comment, as #114's ancillary choices were. No threat-model change ("Failure artifacts"). No README change.

## Tests

The ticket's test is the scenario. Each item below is checked in the PR.

1. `smoke` passes in `chromium`, `firefox` and `webkit` in the `web e2e` job, with steps 1 to 7 as specified. The PR links the run and gives each project's time.
2. The same passes locally on macOS in all three projects (macOS WebKit included), recorded in the PR with the Mac's model.
3. **The checks can fail.** On a throwaway branch, not merged, each of these is made once, and the test fails at the step named:
   - `takeSession` skips `replaceState` (step 2);
   - `Live` leads in every tab, which you get by passing an environment without `locks` (step 4);
   - the overview ignores streamed operations (step 5);
   - the smartlog lays rows out in the server's order instead of its ranks (step 6: `wsA`'s row moves to the top);
   - `NoSession`'s text is changed (step 7).

   The PR lists the five failures, with the uploaded artifact from one of them opened in the trace viewer, to show the artifacts work. This is the evidence that the test tests something.
4. **Stale binaries are caught.** Running Playwright directly against binaries built before an app change fails at step 1 with its message.
5. **Placeholder binaries are caught.** After `just web-clean` and `cargo build`, running Playwright directly fails at step 1 with the `WEB_UI_NOT_BUILT` message.
6. **Isolation.** A run from inside `reef ws run` (with `REEF_WORKSPACE`, `REEF_SOCKET` and the rest set) passes, and touches neither the user's daemon nor its state directory: the user's `reef daemon status` uptime and `reef op log` are unchanged.
7. `npm run lint` and `npm run typecheck` cover `e2e/` and `playwright.config.ts`. A `page.waitForTimeout(1)` or `waitUntil: "networkidle"` in `e2e/` is a lint error, shown once in the PR. Vitest's test count is unchanged (no `e2e` file runs under it).
8. `just web-check` passes. The built `index.html` names the same hashed script as on `main` at the PR's base, so the bundle is byte-identical. `THIRD_PARTY_LICENSES.txt` is unchanged and names no Playwright package.
9. `web` stays under 1.5 minutes. The PR gives its time beside `main`'s 55 s.

## Implementation notes

- **Order**:
  1. the harness and step 1 locally, on Chromium;
  2. steps 2 to 7 on Chromium;
  3. Firefox and WebKit locally on the Mac;
  4. lint, types and ignores;
  5. the recipe;
  6. the CI job, iterated on the PR;
  7. tests 3 to 6 on a throwaway branch;
  8. documentation.
- **Reading the order of changes**: `options.evaluateAll(els => els.map(e => e.getAttribute("aria-label") ?? ""))`, then take each label's `change xxxxxxxx` token with a regex. Poll that whole list with `expect.poll` after the describe, rather than polling one row: the refresh replaces the list in one React commit, and polling the list sees either the old one or the new one.
- **Markers, not navigation events**: `framenavigated` fires for same-document navigations too (the router's `pushState`, `replaceState`), so it cannot tell a reload from a route change. A `window` property survives the second and not the first.
- **The second tab's token**: pages of one context share `localStorage`, so `goto(url)` with no fragment is exactly a middle-clicked link. Open no second context: a context is a fresh profile, and it would show the no-session page.
- **`page.goto` and streams**: the app's streams are `fetch`es started after the document's scripts run, so they do not hold back `load`, and `goto`'s default `waitUntil: "load"` returns. Never wait for `networkidle`.
- **Step 7's two waits**: `Reconnecting` before the new `reef ui`, then the no-session text after it. Between them, the tabs retry at most every 2 s (`live/backoff.ts`), so the second wait is about 2 s after the bind. 30 s covers a debug daemon's start on a loaded runner.
- **`exactOptionalPropertyTypes`**: Playwright's option objects take `undefined` badly under it; build options objects without `undefined` values rather than loosening the tsconfig.
- **Do not add a test id** to the app for this ticket ("Selectors").
- **Linux**: no Rust is touched, so nothing needs the privileged container. The CI job is the Linux run.

## Acceptance criteria

- [ ] `just web-e2e` builds the app and both binaries, and passes `smoke` in Chromium, Firefox and WebKit on Linux (CI) and macOS (by hand) (tests 1, 2).
- [ ] The test checks the issue's five behaviors as the scenario specifies: the token is stripped and stored; the overview lists the repository and a describe updates it live in both tabs; one operation stream and one presence stream for two tabs; the smartlog's row for a described change keeps its position, takes the new description and commit, with no reload; and a restart on a fixed port shows the no-session page and recovers through a new session (test 1).
- [ ] Each check is shown able to fail (test 3), and stale or placeholder binaries are named as such (tests 4, 5).
- [ ] The harness touches no daemon, state directory or configuration but its own (test 6).
- [ ] `web e2e` is a job in `ci-ok`, with artifacts on failure, no retries, and a time recorded warm and cold. `web` stays about a minute (tests 1, 9).
- [ ] The bundle is byte-identical and the licenses file unchanged (test 8). Lint and typecheck cover `e2e/` (test 7).
- [ ] CONTRIBUTING (both places), the CHANGELOG and the phase README are updated. `just web-check` and `just ci` pass.

## Verification

```sh
just web-install                                   # once
(cd web && npx playwright install chromium firefox webkit)   # once
just web-e2e                                       # all three browsers
just web-e2e --project=webkit                      # one
just web-check                                     # lint and types now include e2e/
just ci
```

## Inherited scope

- **From R2.18 and ADR 0047**: the session hand-off (fragment, `localStorage`, `replaceState`), one operation stream and one presence stream per browser through a Web Lock and a `BroadcastChannel`, the no-session page, sessions that die with the daemon, and a listener that binds on first use. Its decision 6 deferred this test to here.
- **From R2.19**: the smartlog at `/repos/<id>/smartlog`, the overview's link, stable row positions across rewrites, the listbox and its accessible names, and the comment on #128 adding the smartlog check. Its decision 5 kept #128 separate.
- **From R2.18's CI decisions**: `check` stays Node-free, the release builds the assets once, and `web` builds and tests the app.
- **From `scripts/test-remote.sh`**: the scratch daemon's shape (a temporary root, configuration before start, a foreground daemon, polling `daemon status`, `--no-autostart`).

## Out of scope

- **A macOS CI job for Safari or WebKit.** The macOS runner is the slowest and costs about ten times a Linux minute, `check` on `macos-15` is the critical path, and even there Playwright's WebKit is not Safari. Local macOS runs are recorded instead.
- **The release workflow.** No browsers on release day (see "Where it runs").
- **The remote listener in a browser** (R2.08: TLS with a pinned key and a token). Playwright could reach it with `ignoreHTTPSErrors`, but that is R2.08's surface, tested across containers by `test-remote.sh`. A follow-up if wanted.
- **The dev server path** (`npm run dev` and Vite's proxy).
- **Leader handoff**: closing the leading tab and checking that the other takes over the streams. W6 tests it in Vitest with fakes. In a browser it depends on how fast the lock is released on page close, which differs by browser. A follow-up if it is ever seen to fail by hand.
- **Visual comparison, accessibility audits (axe), and performance numbers.** Possible follow-ups. This is a smoke test.
- **A daemon-side subscription count** (open question 3).
- **Dependabot for npm** (open question 5).
- **Checks for R2.20's and R2.21's views.** They add steps to this test when they land.

## Risks

- **Browser downloads are a new external dependency of a required check.** An outage of Playwright's browser CDN fails `web e2e` and so `CI passed`. Re-running the job is the remedy, as for an npm or crates.io outage. Should outages recur, the fallback is caching `~/.cache/ms-playwright` ("Toolchain").
- **A browser-specific failure blocks the PR.** The rule in "Browsers" decides what happens, and the user signs off (open question 2).
- **Flakes under load.** The waits are on state, the timeouts are about 40 times the live paths' latency, and only one daemon runs at a time. If the job still flakes, the fix is in the test or the app, not a retry (open question 4).
- **Cold builds.** The job's first run, and every lockfile change, build the daemon's dependencies from scratch, in about 15 minutes against a 30-minute cap. It still finishes before `macos-15`.
- **Port reuse in step 7.** Between the stop and the new bind, another process could take the port. The window is under a second on a runner that runs nothing else. A failure there names the address (`WEB_UI_UNAVAILABLE`), and the step's message says what happened.
- **Playwright's pinned browsers age.** With no Dependabot for npm, 1.64.1's browsers stay as they are until someone bumps it. That is acceptable for a smoke test, and open question 5 addresses it.

## What later tickets inherit

- **`web/e2e/reef.ts`**: a scratch daemon with a fixture repository, a workspace, `reef(...)` with an agent, `session()`, and restart. R2.20 (workspaces and agents), R2.21 (the feed and the scrubber at `?at=`) and R3.31 (the first write from the UI) add their steps to `smoke`, or a spec beside it, on the same fixture.
- **The stream counter**: any later view must keep "one operation stream and one presence stream", and every new view's step asserts it.
- **The `web e2e` job and `just web-e2e`**: the place for any browser check.

## Open questions for the user

All five were decided by the user on 2026-10-31, as recommended.

1. **A new `web e2e` job, or steps in `web` as the issue says?** Steps in `web` are simpler: no second Node setup, no second checkout. But they turn a one-minute job into a 6 to 15-minute one, past its cap, and put the daemon's dependency tree in its cache. A new job keeps `web` fast, has its own cache, timeout and check name, and re-runs alone. Both are off the critical path. Recommendation: a new job, in `ci-ok`, as written. Decided (user, 2026-10-31): as recommended.
2. **All three browsers in CI?** The time allows them, and Firefox and WebKit test separate implementations of exactly the APIs the stream sharing uses. The cost is three times the flake surface, and WebKit on Linux is not Safari. Recommendation: all three, with the rule in "Browsers" for a browser-specific failure, decided with you before merge if it comes up. The alternative is Chromium and Firefox in CI, the two browsers R2.18 listed for Linux, with WebKit local only. Decided (user, 2026-10-31): as recommended.
3. **Expose the daemon's open subscription count?** `GetDaemonStatusResponse` could carry `Subscriptions::active()` (an API revision). That would let this test, `reef daemon status` and a later fleet view see streams from the daemon's side. The browser-side count is exact for this test. Recommendation: no, not for a test. Revisit if R2.20 or an operator view wants the number for its own sake. Decided (user, 2026-10-31): as recommended.
4. **Retries.** Recommendation: none, as written. The repository retries no test, and a pass on retry hides the cross-tab races this test exists to catch. The alternative, `retries: 1` in CI with traces on the first retry (Playwright's default advice), keeps `CI passed` green through one-off flakes, but lets real races through as "flaky". Decided (user, 2026-10-31): as recommended.
5. **Dependabot for npm.** Adding `/web` to `dependabot.yml` would bring weekly grouped PRs for every web dependency, Playwright and its browsers included. It would need an ignore for `typescript` above 6.0 until typescript-eslint supports it. Recommendation: a follow-up issue filed by this PR, kept out of it, so this PR changes only tests and CI. Decided (user, 2026-10-31): as recommended.
