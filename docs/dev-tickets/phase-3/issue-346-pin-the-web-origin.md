# Issue #346: pin the web origin, so a passkey touch counts only on Reef's own page

Phase 3 · Size M, at its top end (about 2,600 lines with tests and prose) · Fixes: #346 (first-review F2 and third-review T1 of #338) · Depends on: R3.43 (passkeys, approvals, `[passkeys]`), R2.18 (the web listener), and builds on R3.31 (a web landing's touch), R3.39 (the desktop app loads the page at its origin), #260 (a default that differs by state directory) · Blocks: R3.40 (the phone's passkeys) and the next release · Follow-ups: listed at the end

## Context

**The goal.** A passkey approval counts only when the ceremony ran on Reef's own web page.

**The problem.** A passkey's RP ID is `localhost`, so the browser lets a page at any `http://localhost:<port>` ask the person's passkey for an assertion. The running daemon accepts an assertion only from its own origin. A certificate does not: its reader checks the origin's host and ignores the port, because the port changes at every daemon start. A process that runs as the person can read `<state>/identity/authority.pem` (S-58). So it can:

1. Show a look-alike approval page on another port, get one touch over a challenge it derived itself, and sign the certificate with the authority key. The certificate verifies everywhere, the passkey is pinned, and the sign-off counts (S-255).
2. Show a look-alike Passkeys page whose snippet names the agent's fingerprint, and wait for the person to paste it into the first pin change (S-250, S-255).

With the default port 0 the address bar gives the person no cue. Today the only defence is documentation: open Reef's pages only with `reef ui` (`docs/policy.md:159`, `docs/identity.md:179`).

**The smallest change that meets the goal.** Give the listener a port that does not change. Put the origin beside each fingerprint in `[passkeys]`. Make the one presence function compare both. Everything else here exists because a fixed port brings its own problems, which this brief found by measuring them.

### The issue text, against the code

| The issue says | The code says |
| --- | --- |
| "The web listener accepts a WebAuthn ceremony from any `http://localhost:<port>` origin" | It does not. The daemon compares the whole origin at registration (`crates/reef-daemon/src/identity/passkeys.rs:322`) and at every assertion (`passkeys.rs:490`), and tests hold it (`crates/reef-daemon/tests/passkeys.rs:529` to `:544`, `:1051` to `:1071`). The gap is only the certificate's reader (`crates/reef-evidence/src/webauthn/mod.rs:626`), reached by an attacker that holds the authority key and skips the daemon. |
| "the certificate reader checks the host, not the port" | Correct: `origin_problem` (`webauthn/mod.rs:626`), called from `client_data` (`:614`). |
| A look-alike can "show a passkey snippet with an agent's fingerprint without any touch" | Correct, and **an origin pin does not fix it**. See "The two halves of the issue". |
| "an assertion from any other origin ... certifies nothing" | Too strong as written. Whoever holds the authority key can still make a certificate that verifies. With this ticket that certificate's presence counts as `none` under every policy that pins. |

### What was verified while writing this ticket

Reef at `60211bdc`: API revision 46 (`crates/reef-api/src/lib.rs:49`), schema 29, ADR 0093 the latest, the threat model's last row S-273. This ticket takes **API revision 47**, **ADR 0094** and **S-274 to S-278**. It needs **no migration**, so schema 30 stays free. Line numbers are at that commit.

**How it works today: the listener.**

- `[ui] listen` defaults to `127.0.0.1:0` (`crates/reef-daemon/src/config.rs:1311`). `UiSection::listen_port` (`config.rs:1319`) accepts `127.0.0.1:<port>` or `[::1]:<port>`, and either means both families.
- Nothing binds until the first `CreateWebSession` (`web/mod.rs:238`, ADR 0047 decision 2). `bind` (`web/mod.rs:328`) takes both loopback families on one port (`Loopback::bind`, `:393`) and records the origin (`:337`). A failed bind is `WEB_UI_UNAVAILABLE` with no fallback port; its hint suggests port 0 (`:163`).
- The gate compares `Host` with `localhost:<port>` exactly (`web/gate.rs:141`), then `Origin` and `Sec-Fetch-Site` for API calls (`gate.rs:166`), then the bearer token. The app keeps the token in `localStorage` (`web/src/session.ts:71`), which is per origin.
- ADR 0047 rejected "Binding at start" and "A fixed default port. Another user can take a known port first". S-31 already lists what a fixed port opens, "Open (Medium, fixed ports only)": a squatter's page can register a service worker that later reads the session token.
- Precedent for a default that depends on the state directory: `workspace.root` goes under `~/.reef` only for the default state directory (`config.rs:1832`, from `Layered::home`). A daemon started with `REEF_STATE_DIR`, `--state-dir` or `Config::with_defaults` (`config.rs:1863`) keeps its own.

**How it works today: the certificate and the policy.**

- `check_assertion` (`webauthn/mod.rs:325`) returns `Asserted { origin, sign_count }`. `PresenceEvidence::check` (`:550`) returns it too. The certificate's reader drops it: `presence_proof` (`session.rs:1025`, the call at `:1057`) keeps the evidence and not the `Asserted`. `certify` (`session.rs:569`) parses its own payload back through the same check.
- The origin is inside the certificate already: `presenceEvidence.clientDataJSON` is what the passkey signed and what the authority signed over.
- `Policy::presence` (`crates/reef-policy/src/eval.rs:479`) is the one function every reader uses: attestor sets, `[[signoff]]`, `[owners]` and CEL (`eval.rs:716`), a web landing (`landing/touch.rs`, `pinned`), and `reef evidence show` (`crates/reef-vcs/src/cli/evidence.rs:1177`). It compares the fingerprint and the identity.
- `[passkeys]` is `BTreeMap<String, Spanned<List>>` (`format.rs:112`), each value a list of 64-hex fingerprints (`format.rs:1831`).
- `Key` reaches policy in two places: `key_of` in `crates/reef-daemon/src/service/policy.rs:466` and `pinned_at_trunk` (`cli/evidence.rs:1188`).
- A web landing also needs the passkey live in the daemon's database (`landing/touch.rs`, `live`).
- The two snippet writers are `passkeys_snippet` (`crates/reef-vcs/src/cli/identity.rs:1234`) and `policySnippet` (`web/src/views/Passkeys.tsx:51`).

**What has shipped.** The `reef-policy` and `reef-evidence` crates are not in the latest tag (`git ls-tree v0.4.2 crates/`), so neither `[passkeys]`, nor policy format 1, nor a passkey certificate is in any release. The user's dogfood daemon (API revision 46) lists no passkeys (`reef identity passkey list` prints `[]`), and neither of its registered repositories has a `reef-policy.toml` at `HEAD`. There is nothing to migrate.

**Spikes** (Playwright, Chromium 156.0.8078.4 headless and WebKit 27.2, on macOS; nothing committed). An "attacker" HTTP server holds a port, then stops; a "Reef" server starts on the same port.

| Question | Chromium | WebKit |
| --- | --- | --- |
| One passkey (virtual authenticator), two ports: does the second port get an assertion? | Yes; `clientDataJSON.origin` names the second port | no virtual authenticator |
| A service worker the attacker registered: who answers the first navigation to `/#session=…` afterwards? | the attacker's worker, and its request URL includes the fragment | the same |
| Does it go away if Reef answers the browser's update check with 404, or with HTML? | No (3 of 3 navigations intercepted) | No |
| What does the update check look like at the server? | `GET /sw.js` with `Service-Worker: script` after each intercepted navigation, never through the worker | the same |
| Does it go away if Reef answers that request with a worker that unregisters itself? | Yes: the second navigation reached Reef | Yes |
| A page the attacker cached for a year: served from cache later, with no request to Reef? | Yes | Yes |
| The same path with a query never used before, or a reload? | Reef's page | Reef's page |
| A hashed asset the attacker cached: does Reef's own fresh page run it? | **Yes** | **Yes** |
| `Clear-Site-Data: "cache"` on Reef's page evicts that asset before it runs? | **No** | Yes |

| Sockets on macOS, one uid (`SO_REUSEADDR`, as tokio sets it) | Result |
| --- | --- |
| Binding `0.0.0.0:p` or `[::]:p` while the daemon holds `127.0.0.1:p` | succeeds; loopback connections still reach the daemon |
| Binding `127.0.0.1:p` and `[::1]:p` while another process holds both wildcards | **succeeds**, so the daemon's bind reports nothing; the daemon gets the connections |
| Connecting to `127.0.0.1:p` and `[::1]:p` before binding | answered by the wildcard holder, so a probe sees it |
| Binding `127.0.0.1:p` twice | `EADDRINUSE` |

Not verified: Firefox (it does not launch on macOS 27, #143), Linux's wildcard rules, two different uids on macOS, and real Safari or Chrome profiles.

## Decisions

The user accepted all three recommendations on 2026-11-22. Section "Decisions for the user" keeps the question, the alternatives and their cost.

1. **The port.** A well-known default, `127.0.0.1:17333`, for a daemon in the default state directory. Port 0 stays the default everywhere else.
2. **Holding it.** A daemon with a fixed port and at least one live passkey binds the web listener at start. Every other daemon binds at the first `reef ui`, as today.
3. **What a squatter leaves behind.** The three browser leftovers the spikes found are closed in this ticket.

Settled by the code, and not asked: the pin's grammar, that a pin without an origin does not load, who compares it, that no fallback port exists, and that nothing is migrated.

## Design

### The data shape

- **An origin** (`reef_policy::Origin`) is the text a browser writes in `clientDataJSON.origin`, in its one canonical form: `http://localhost[:port]`, or `https://<dns name>[:port]`. The name is lowercase labels as `is_rp_id` reads them, with a last label that is not all digits (so no IP literal). The port is 1 to 65535 with no leading zero, and is absent when it is the scheme's default (80, 443). No path, no trailing slash, no user, no wildcard.
- **A pin** (`reef_policy::Pin`) is `{ fingerprint, origin }`. `[passkeys]` becomes `BTreeMap<identity, BTreeSet<Pin>>`.
- **A verified origin** is derived from the certificate, never stored twice: `PresenceEvidence::origin()` reads it from `clientDataJSON`. It means something only on a binding that `check_certificate` returned.

### The pin

```toml
[passkeys]                                 # pinned passkeys, by identity name or id
alice = [
  { fingerprint = "3c9e0d…64 hex…", origin = "http://localhost:17333" },
]
```

- Both keys are required. Any other key is an error. A bare string, the form R3.43 wrote, is a load error at its line and column that says what to write: `a pin names a fingerprint and an origin: { fingerprint = "3c9e…", origin = "http://localhost:17333" }`.
- The format version stays 1. Unknown or wrongly shaped tables already fail closed (ADR 0061 decision 1), and no release reads either form, so there is no reader to protect.
- **A key counts as `user_verified` only when one pin under its identity's id or name has both its passkey's fingerprint and its certificate's origin, byte for byte.** `Policy::presence` is the only place that changes. A pin written in a form a browser never sends cannot match, so the grammar check catches typing mistakes and carries no security weight.
- One passkey at two origins is two pins. A passkey synced between two machines that both use the default port needs one.
- **`Key.passkey_origin`** joins `Key.passkey` in the policy input (empty without a verified presence), and CEL reads it as `e.key.passkey_origin`.

### Who checks what

| Reader | Checks | Changes |
| --- | --- | --- |
| The gate (`web/gate.rs`) | `Host`, `Origin`, `Sec-Fetch-Site`, token | no pin: it serves one origin and cannot know a policy |
| Registration and approval (`passkeys.rs:322`, `:490`) | the exact origin of this daemon's listener | none |
| `certify` and `check_certificate` | type, challenge, the origin's host is the RP ID, flags, signature | none, except that the binding now reports the origin. They hold no policy, and an attacker with the authority key does not need Reef's `certify` |
| `Policy::presence` | fingerprint **and origin** pinned for the identity | the change |
| Attestor sets, `[[signoff]]`, `[owners]`, CEL, `builtin:resolution` | effective presence | follow from `Policy::presence` |
| A web landing (`landing/touch.rs`) | a live, pinned passkey's touch | a new `missing` value, `pinned_origin` |
| `reef evidence show`, the web pages | what the policy says | show the origin |
| A reader with no policy | the authority's word | shows the origin and claims nothing about it |

`VERIFIED_SIGNOFF_REQUIRED` with `missing` `pinned_origin` means that a live, pinned passkey touched this tree at an origin the policy does not pin. That is either a port the person changed or the trace of a look-alike page. The message names both origins and says so, and the daemon logs it as a warning under `reef_daemon::identity::audit`.

### Where the origin shows

- `reef evidence show`: `presence user_verified (passkey "MacBook Touch ID" at http://localhost:17333, pinned here)`, or `(… at http://localhost:49953, an origin this repository's policy does not pin)`.
- `reef policy eval` and `EvaluatePolicy.verified`: each signer's origin, and why it is not pinned (`passkey` or `origin`).
- `reef identity passkey list` and the Passkeys page: the snippet names this daemon's origin. The CLI takes it from `GetDaemonStatus` (below). The page takes it from the origin its confirming touch verified at, which `assertionVerifies` already compares with `window.location.origin` (`web/src/sign/verify.ts`).
- The approval page and Sign off name the origin beside the identity, so the address bar and the page agree.
- `reef daemon status` prints the web UI's origin and whether this daemon holds it.

On a port-0 daemon a ceremony still works, and a policy may pin the origin of that run; the tests do. The CLI and the Passkeys page then print, in place of the copy button, that this origin ends when the daemon stops, and name `[ui] listen`.

### The fixed port (decision 1)

- `Config::load_with` sets `ui.listen` to `127.0.0.1:17333` when the file does not set it and the state directory is the default one (`layered.state_dir_source == Source::Default`), beside the same rule for `workspace.root` (`config.rs:1832`). `UiSection::default()` stays `127.0.0.1:0`.
- **17333** is outside both ephemeral ranges (macOS 49152 to 65535, measured; Linux's default 32768 to 60999), in a block `/etc/services` lists as unassigned (17236 to 17728), and not on Chromium's blocked-port list (whose highest entry is 10080, from memory and not checked). It spells REEF on a keypad after a 1.
- **Tests, scratch daemons, the SDKs' test daemons, the benchmarks** use their own state directory, so they keep port 0 with no change (`crates/reef-daemon/src/testing.rs:68`, `web/e2e/reef.ts:86`, and `REEF_STATE_DIR` throughout `crates/reef-vcs/tests`). The e2e that restarts a daemon already writes a fixed port (`web/e2e/reef.ts:210`).
- **The dogfood daemon** is in the default state directory, so it takes 17333.
- **A second daemon that wants passkeys across restarts** sets its own fixed port and its policy pins that origin.
- **Another user's daemon** on the same machine finds the port taken and sets another. The hint says so.
- **SSH.** `reef ui` over SSH prints `ssh -L <port>:127.0.0.1:<port>` (`cli/ui.rs`, `run`). With both machines on the default port, the laptop's own daemon holds 17333, so that command now fails. The note says so and names `[ui] listen` on the remote machine. The forwarded port cannot differ, since the gate compares the whole `Host`.
- **`[ui] listen` in `config.toml` wins**, port 0 included.
- Both loopback families stay bound on the one port, and either one taken is an error (S-251, unchanged).

**When the port is taken**, the web UI does not start, and there is no fallback port: a fallback would teach the person that Reef's port varies. The daemon and every other command work. `WEB_UI_UNAVAILABLE`'s hint changes from "or set port 0" (`web/mod.rs:163`) to: find the holder (`lsof -nP -iTCP:17333 -sTCP:LISTEN`); if it is not a daemon of yours, do not touch a passkey on any page at that address; to move Reef, set another fixed port and pin its origin.

**The probe.** Before it binds a fixed port, the listener connects to `127.0.0.1:<port>` and `[::1]:<port>` with a 250 ms limit. An answer is `WEB_UI_UNAVAILABLE`, "another process answers there". Without it, a wildcard listener goes unseen on macOS (measured above): it serves nothing while the daemon runs, and serves the pinned origin the moment the daemon stops.

### Holding the port (decision 2)

- At start, after the database opens and the socket listens, a daemon with `[ui] enabled`, built assets, a fixed port and **any live passkey** binds the listener through the same `bind`, behind the same mutex. A failure is a warning in the log and in `reef daemon status`, and the next `CreateWebSession` tries again. It never fails the start.
- The first registration happens through the listener, so from the first passkey on, the port is held whenever the daemon runs.
- A daemon whose user never adds a passkey and never runs `reef ui` still opens no TCP port, which is the promise in ADR 0047 decision 2 that matters.
- No new task or timer: the hold is the listener that already exists.

### What a squatter leaves in the browser (decision 3)

With a random port, whatever a browser kept for `http://localhost:<port>` was dead at the next start. A fixed origin keeps it. The spikes found three things that outlive a squatter and would put its script on Reef's own page, at the pinned origin, with the daemon running:

1. **A service worker.** The gate answers any request with `Service-Worker: script` (right after the `Host` check, before the assets) with a worker that calls `skipWaiting` and unregisters itself. It ends every web session, since any token handed to that origin may have gone to the worker's page, and logs a warning. Reef's page registers no worker, so no honest browser sends this. `worker-src 'none'` joins the CSP (`gate.rs:48`).
2. **A cached page.** `CreateWebSession` adds `?launch=<16 hex>` to the session URL (`web/mod.rs:269`), a URL no cache holds. The app removes it with the fragment (`session.ts`, `replaceState`).
3. **A cached asset.** `Clear-Site-Data` does not help in Chromium, so the URLs change: the daemon serves assets under `/a/<16 hex of this run>/assets/`, and `index.html` names them there. Vite's `base` becomes `./`, so a chunk's own imports stay under the prefix. The unprefixed `/assets/` is 404 in the daemon. `index.html` stays `no-store` (`web/assets.rs:27`).

One thing stays open: **a squatter's page still open in a tab**. It shares the origin, so it can read `localStorage` when the next session stores its token. It ends when the tab closes. See "What remains open".

The first navigation after a worker was registered still runs the worker's page: the kill arrives with the update check that follows it. That page can get one touch. The kill keeps it from being permanent.

### What a fixed port changes elsewhere

- **ADR 0088's rejected `localStorage`.** One of its two reasons, "resets at every daemon restart (the port changes)", is gone in the default state directory. The other, that storage differs between the browser, the desktop app and the phone, stands, and `WatchInbox` (ADR 0091) relies on the daemon's mark. The decision does not change; ADR 0088 gets a note. `reef.devMode` now survives a restart.
- **Web sessions at a restart.** Sessions stay in memory and end (S-31). A tab that was open reports `WEB_SESSION_INVALID` from the same origin rather than a dead connection, and works again after `reef ui` and a reload, since the new token is in the shared `localStorage`. Sessions are still never persisted: whoever took the port in between would collect a live one.
- **Two daemons at one origin, one after the other.** They share `localStorage`. The second receives the first's token, which died with the first.
- **Cookies** ignore ports, so any `localhost` server's cookies reach Reef. Reef sets and reads none.
- **DNS rebinding.** A rebound page sends its own name as `Host`, so the gate answers 421 (S-30). Nothing changes.
- **CSRF.** Every site now knows the origin without a port scan. Steps 2 to 7 of the gate do not depend on the port being secret: no CORS headers, `OPTIONS` refused, `Origin` and `Sec-Fetch-Site` checked, a bearer token required. Two new nuisances. A site can tell that something listens on 17333 (accepted, Low). And a link to `http://localhost:17333/#session=x` overwrites the stored token (`session.ts:71` writes before the daemon has seen it): so the app keeps a fragment's token in memory and stores it only after the daemon accepts a call with it.
- **The desktop app.** It gets its URL over the socket and compares origins for equality (`crates/reef-shell/src/lib.rs`, `page_of`, `open`), so a fixed port changes nothing. `page_of` drops the query, so `?launch=` does not reach a path. Its webview cannot run a ceremony (ADR 0093 decision 8); the hand-off opens the browser at the same origin. If the port is taken, the window shows the same words as `reef ui`.
- **The Vite dev server** still cannot run a ceremony.
- **SDKs and MCP** assume no web URL: the SDKs regenerate, and MCP's tools are unchanged.

### R3.40's seam

The pin already holds `https://<dns name>[:port]`. In a certificate the origin's host must equal `rpId` (`webauthn/mod.rs:626`), so a pinned origin fixes the RP ID without another key. `verify_assertion` reads the RP ID from the passkey's row (`passkeys.rs:489`). `approve` writes the constant (`approvals.rs:607`); this ticket makes it `passkey.rp_id`, so R3.40 changes only where a passkey's RP ID and the channel's origin come from. Nothing remote is built here: the remote listener still refuses passkey and approval writes (`PASSKEY_OVER_REMOTE`).

R3.40 also changes what the pin is worth. At `https://<dns name>` the browser checks a TLS certificate. When the daemon and its TLS key are on another machine or under another uid, a process on the person's laptop cannot serve the pinned origin at all.

### Departures

The review of the implementation broke "What a squatter leaves in the browser" in Chromium, and the implementation departs from it: a launch clears the pinned address from a page on a second, kernel-chosen loopback port (ADR 0094 decision 5), Reef's page removes every worker and cache, a failed hold is tried again, streams end with their sessions, and a token stays in its tab until it is accepted. Test 7 (a daemon moved to another port) is covered by the attack test's end, which pins the other origin and lands; test 17's restart by `a_passkey_makes_the_daemon_hold_its_fixed_port`. S-275's "certifies nothing" was wrong: a script at the pinned origin with a session gets one misled touch certified by the daemon.

## What the pin stops, and what it does not

The attacker's position decides. "Counts" means presence `user_verified` under a policy that pins.

| Attacker | Today | With this ticket |
| --- | --- | --- |
| A web page, another user, or a sandboxed process that cannot read `authority.pem` or reach the socket, on **another** port | a touch approves nothing: it cannot sign a certificate, and the daemon refuses the origin | the same |
| The same, **at the pinned port** while the daemon is down | not possible to aim at (random port) | gets an assertion and still cannot certify it. It can serve a page there, and take a later session's token if its tab stays open (S-275) |
| A same-uid process, look-alike page on **another** port | **counts** | **does not count**, at any port but the pinned one, whatever it signs with the authority key |
| A same-uid process **at the pinned port** | not needed | **counts**, if the person touches there. It must first take the port: stop the daemon, or catch it down, or move it with `config.toml` |
| A same-uid process that controls **the page or the browser** (a replaced `reef-daemon`, a browser extension or automation it drives, a patched profile) | counts | **counts** |
| Anyone, with no touch by the person | does not count | does not count |

So the pin does not make a person's touch safe from code that runs as that person. ADR 0027 says that is not a boundary. The pin takes away the cheap attack: any port, at any time, with the daemon running and no trace. What is left needs the port, the daemon or the browser. Taking the port leaves traces: the daemon is down or on another address, `reef ui` fails with `WEB_UI_UNAVAILABLE`, `reef daemon status` says the port is held by another process, and the log has it.

**Nothing on one uid closes the rest.** A same-uid process can read `authority.pem`, stop the daemon, edit its configuration, and serve plain HTTP on any port above 1023. A TLS key on the machine would be readable the same way. What closes it is a page and an authority the agent's uid cannot touch: agents in a sandbox without the state directory and the socket (S-07), or an `https` origin served elsewhere (R3.40).

### The two halves of the issue

**"Any same-uid process can read `authority.pem`."** Still true. Closed for look-alike pages on another port: the certificate such a process signs does not count. Not closed at the pinned port, and not closed for a reader with no policy, which still takes the authority's word.

**"The first pin change is judged by the policy in place before it."** Not closed, and an origin cannot close it. The look-alike snippet can name the correct origin with the agent's fingerprint, because an agent can register a software passkey through the real page with a headless browser (S-250). Nothing exists before the first pin that could judge it, except the person. What this ticket changes: an origin other than the daemon's own is visible in the line itself, and the default one is a constant the person can know. What to do is unchanged and stays documented: take the line from a page you opened with `reef ui`, and compare the fingerprint with `reef identity passkey list` in your own terminal.

### What remains open

- A same-uid process that takes the pinned port and gets a touch there (S-274).
- A same-uid process that controls the browser or replaces the daemon (S-274).
- The first pin (S-250).
- A person who touches without reading (S-254).
- A squatter's tab left open, which can read the next session's token (S-275), and the one navigation before a worker is killed (S-276).
- A page cached by a squatter and opened from a bookmark or a typed path, which carries no `?launch=` (S-276). A reload fetches Reef's page (measured).

## API, schema and formats

- **API revision 47**, additions only:
  - `SessionKey.passkey_origin` (string, next free field): the origin its assertion ran at, from the certificate; empty otherwise.
  - `PolicyVerifiedSigner.origin` (string) and `.unpinned` (string: empty, `passkey` or `origin`), beside `pinned`.
  - `GetDaemonStatusResponse.web_ui`: `WebUiStatus { state, origin, problem }`, with `state` one of `DISABLED`, `NOT_BUILT`, `UNBOUND`, `BOUND`, `UNAVAILABLE`. `origin` is the bound origin, or a fixed port's before it is bound, or empty for an unbound port 0.
  - `VERIFIED_SIGNOFF_REQUIRED`'s `missing` gains `pinned_origin`, with metadata `origin`.
  - No new RPC, reason or access level. `API_DIGESTS` gains its line; the SDKs regenerate.
- **Schema: no change.** The origin is in the certificate. No column, no `0032` file, the version stays 29.
- **Policy format: version 1, `[passkeys]` as above.** `reef schema policy` and `reef schema policy input` follow.
- **Certificate format: no change.**

## Deliverables

- **`crates/reef-policy/`**: `Origin` and `Pin` in `format.rs` (the bare-string error, the grammar, duplicates); `passkeys: BTreeMap<String, BTreeSet<Pin>>`; `Policy::presence` and `Policy::verified` in `eval.rs`; `VerifiedSigner.origin`, `.unpinned`; `Key.passkey_origin` in `input.rs`; `schema.rs`; the starter's comment (`lib.rs:138` to `:142`); the corpus (`passkeys.toml`, its inputs, one new case for a wrong origin).
- **`crates/reef-evidence/`**: `PresenceEvidence::origin()`; the module documentation, which says what a stateless reader cannot know (`webauthn/mod.rs:19` to `:24`).
- **`crates/reef-daemon/`**: `config.rs` (the default by state directory, its documentation); `web/mod.rs` (hold at start, the probe, `WebUiStatus`, `?launch=`, the asset prefix, the hint); `web/gate.rs` (`Service-Worker`, `worker-src 'none'`); `web/assets.rs`; `web/session.rs` (end all); `web/launch.rs` (its comment); `daemon.rs` (step 10's documentation and the hold); `identity/read.rs`, `service/policy.rs` (`passkey_origin`); `identity/approvals.rs:607`; `landing/touch.rs` (`pinned_origin`); `identity/mod.rs:541` (the hint names the origin).
- **`crates/reef-api/`, `crates/reef-client/`, `crates/reef-vcs/`, `sdk/`**: revision 47; `cli/identity.rs` (`presence_text`, `passkeys_snippet`); `cli/evidence.rs` (`pinned_at_trunk` returns why); `cli/policy.rs`; `cli/daemon.rs` (status); `cli/ui.rs` (the SSH note); the output schemas.
- **`crates/reef-web/`, `web/`**: Vite's `base: "./"`, `tests/assets.rs`; `session.ts` (store after acceptance, strip `launch`); `Passkeys.tsx` (`policySnippet` with the origin, the port-0 words); `Approve.tsx`, `SignOff.tsx`, `Proposal.tsx` (the origin, why unpinned); `proposal-actions.ts` (`pinned_origin`).
- **`crates/reef-shell/`**: a test that `?launch=` does not reach a path. No other change.
- **Documentation**:
  - **ADR 0094** "The pinned web origin": the pin, the default port, the hold, the leftovers, what is not closed. It amends 0047 (decisions 2 and 4, and the two rejected alternatives), 0061 (`[passkeys]`), 0085 (decisions 2, 4 and 9, and the look-alike consequence) and adds a note to 0088. The ADR index.
  - `docs/policy.md`: `[passkeys]` in "The file" and "Pinned passkeys" (`:43` and `:145` to `:163`), `e.key.passkey_origin`. `docs/identity.md`: "Passkeys and verified presence" (`:146`, `:148`, `:172` to `:181`, `:239`, `:244`, `:260`, `:279`, `:280`). `docs/evidence.md`, `docs/proposals.md` (`pinned_origin`), `docs/quickstart.md:153`, `docs/desktop.md`, `docs/landing.md`.
  - `crates/reef-daemon/README.md:191`, `README.md:67` and `:105`, `CONTRIBUTING.md:120`, `CHANGELOG.md` (Unreleased: the port, the pin's form, sign-offs made before it).
  - `docs/threat-model.md` (below), `docs/dev-tickets/phase-3/README.md` (R3.40's row inherits the seam).

## Threat model

Changed rows:

- **S-31** (port squatting): the default is now a fixed port in the default state directory. "Open (Medium, fixed ports only)" becomes S-275 and S-276, and the sentence that binding at start "is not done" goes.
- **S-250**: the pin names an origin; the first pin is still the person's check.
- **S-251**: unchanged in substance; names the probe.
- **S-252**: a reader that pins now checks the whole origin.
- **S-254**: a look-alike on another port counts for nothing under a pin.
- **S-255**: closed for policy at any origin but the pinned one; points to S-274 for the rest.

New rows:

- **S-274 A ceremony at the pinned origin, not on Reef's page** (a same-uid agent, High). It stops the daemon or catches it down and serves the pinned port, or it controls the page through the browser or a replaced daemon. Bounded: the port is held from daemon start once a passkey exists; a taken port is never replaced by another; status, log and `reef ui` report it. Documented: on one uid nothing prevents it (ADR 0027).
- **S-275 Another local user squats the known port** (another local user, Medium). While no daemon holds it, they serve a page there; it certifies nothing, but a tab left open shares the origin and can read the next session's token, which reads every registered repository. Bounded: the hold, and no web session is minted while the port is taken. Documented: on a machine with other local users, close a tab that showed an unexpected page, or set a port of your own.
- **S-276 What a squatter leaves in the browser** (as S-274 and S-275, Medium). A service worker, a cached page, a cached asset. Fixed: the worker answer, the launch URL, the per-run asset paths. Documented: the first navigation before the kill, and a bookmarked path.
- **S-277 A port every site knows** (a page in the user's browser, Low). Any site can tell that 17333 answers, and can navigate there with a fragment. Bounded: the gate; the token is stored only once accepted.
- **S-278 A listener the bind does not see** (a same-uid agent, Medium). On macOS a wildcard listener coexists with the daemon's and takes over when it stops. Bounded: the probe before each bind. Documented: one started after the bind is seen only at the next start.

"Loopback listener (R2.18)" and "Boundaries" follow (`docs/threat-model.md:20`, `:65`, `:385` to `:402`).

## Migration

- **Policies.** A `[passkeys]` with bare fingerprints does not load, so its destination is `invalid_policy` and lands nothing until someone commits the new form by hand, as any broken policy is fixed (`docs/policy.md`, "Where the policy comes from"). No release reads the old form, and the dogfood repositories have none. No compatibility path is built: a bare fingerprint meaning "any origin" would be the gap itself.
- **Passkeys.** They stay. The RP ID `localhost` has no port, so a passkey registered at any port asserts at 17333.
- **Certificates.** They stay valid. A sign-off made at another port counts as `none` under a new pin; an open proposal needs a new touch. Landed changes are not judged again.
- **Bookmarks and `localStorage`.** Old random-port URLs were already dead at each restart.
- **A config file that names `127.0.0.1:0`** keeps it.
- **Downgrade.** A pre-ticket build from `main` refuses the new form as a load error, so it lands nothing. It fails closed.

## Tests

In `crates/reef-policy/`:

1. `[passkeys]` parses pins; a bare string, a missing key, an unknown key, an empty list, a malformed fingerprint and each malformed origin (no scheme, `http` with a name other than `localhost`, an IP literal, an upper-case name, a trailing slash, a path, port 0, port 80 written out, a leading zero, 65536) is an error at its line and column. `https://reef.example`, `https://reef.example:8443` and `http://localhost:17333` load.
2. `verified_presence_counts_only_with_a_pinned_passkey` gains the origin: the right fingerprint at another port, at `https://localhost:17333`, or with an empty origin counts as `none` in an attestor set, a `[[signoff]]`, `[owners]` and CEL; two pins of one passkey accept both origins; a pin under another identity does not.
3. `verified` reports `origin` and `unpinned` (`passkey`, `origin`, empty). The corpus and the schemas agree.

In `crates/reef-evidence/`:

4. `PresenceEvidence::origin()` is the origin in the checked `clientDataJSON` for the ES256, EdDSA and RS256 vectors and Chromium's own bytes. `check_certificate` still accepts any port of the RP ID's host, asserted so that nobody later believes it does not.

In `crates/reef-daemon/` (`tests/passkeys.rs`, `tests/web_ui.rs`, `config/tests.rs`):

5. **The attack.** `a_ceremony_on_another_port_certifies_nothing`: a policy pins alice's passkey at the page's origin. The test derives an approval challenge, has her authenticator assert it from `http://localhost:<another port>`, certifies a key with the daemon's own `authority.pem`, writes the key's row as the daemon would (`Db::insert_session_key`, `db/identity.rs:655`), and adds the sign-off. The certificate verifies and the evidence is stored. `EvaluatePolicy` reports `pinned` false, `unpinned` `origin`; `reef land` blocks; a web landing is refused with `missing` `pinned_origin`. The control: the same steps at the pinned origin count and land.
6. The daemon's own path still refuses it: an assertion from another port at `ResolveApproval` and `RegisterPasskey` is `PASSKEY_ASSERTION_INVALID`, check `origin` (exists; kept).
7. A daemon restarted on another fixed port: its sign-offs are `pinned_origin`, and count again after the policy pins the new origin.
8. The default: the default state directory gives `127.0.0.1:17333`; a state directory by flag or environment gives port 0; a config file wins either way. `reef config show` prints it.
9. The hold: a daemon with a live passkey and a fixed port is bound after start with no `CreateWebSession`; with no passkey, a removed one only, port 0, `[ui] enabled = false` or placeholder assets it is not. A taken port is a warning, a running daemon, `web_ui.state` `UNAVAILABLE`, and a later `CreateWebSession` binds once it is free.
10. The probe: a process holding the IPv4 or the IPv6 wildcard at the port makes the bind `WEB_UI_UNAVAILABLE` on macOS and Linux.
11. `Service-Worker: script` on any path gets the unregistering worker with a JavaScript type and the security headers, ends every session, and logs; a request with a wrong `Host` gets 421 first. Every response's CSP has `worker-src 'none'`.
12. Assets: the index names them under this run's prefix, the prefix serves them, `/assets/…` is 404, and a second daemon's prefix differs. The session URL has a fresh `launch` each time.

In `crates/reef-vcs/`:

13. `reef evidence show` and `reef policy eval` print the origin and why a signer is not pinned; `reef identity passkey list` prints a snippet that `Policy::load` accepts, with the fixed origin, and the port-0 words without one; `reef daemon status`; schema validation.

In `crates/reef-shell/`:

14. A URL with `?launch=` maps to its path.

In `web/` (Vitest):

15. `policySnippet` with the origin, byte for byte as the CLI's; the Passkeys page on a port-0 origin; a fragment's token is stored only after an accepted call, and a refused one leaves the stored token; `launch` is stripped; `pinned_origin`'s words.

`web/e2e/`:

16. **The attack, in a browser** (Chromium, CDP virtual authenticator): a second local server on another port asks for the same passkey's assertion over a pending approval's challenge and gets it; submitting it through Reef's page is refused with the origin named, and the approval stays pending.
17. The existing passkeys flow with the origin in the snippet's expected text (`e2e/passkeys.spec.ts:143`), on a fixed port across a restart: the pin still counts.
18. **The leftovers** (Chromium and WebKit): a squatter on the port registers a service worker and caches `/`, `/passkeys` and the app's asset URLs, then stops. A daemon starts on that port. The first launch may show the squatter's page; the second shows Reef, runs no cached script, and holds no worker.

Linux: 9 to 12 run in the Docker container; the IPv6 half skips where there is no `::1`.

## Implementation notes

- **Order.** (1) `reef-policy`'s pins with tests 1 to 3. (2) `PresenceEvidence::origin()`, the readers, `touch.rs`, revision 47, with test 5 failing first on `main`'s behaviour. (3) The CLI and web text. (4) The default port, the hold, the probe, status. (5) The leftovers. (6) The e2e. (7) The documents. Steps 1 to 3 meet the goal and each step leaves the tests green.
- **Subtract.** The `pinned: bool` plumbing in `cli/evidence.rs` and the web rows becomes one value that says why. The documentation's "open Reef's pages only with `reef ui`" paragraphs shrink to the first pin.
- **Do not** add an origin column, a second origin parser in `reef-evidence`, or an "extra origins" setting.
- Deserialize a pin from `Spanned<toml::Value>` so that a bare string gets the message above and not serde's.
- The bind at start must not hold up the socket: spawn it after the socket listens.
- The probe must not treat the daemon's own earlier run as a squatter: it runs only when this process has not bound.

## Performance and laptop cost

- The hold is one bound socket pair and no task beyond the server that `reef ui` already starts. No timer, no poll.
- The probe is two loopback connects, once per bind. A refused connect returns at once.
- Reading an origin is one JSON parse of `clientDataJSON` (under 300 bytes) per verified certificate read.
- Assets are not cached across daemon restarts: at most the budget's 600 KiB over loopback, once per run.

## Acceptance criteria

- [ ] A sign-off certified from a touch at any origin but a pinned one counts as `none` in every reader that has a policy (tests 2, 5, 7).
- [ ] A pin without an origin does not load (test 1).
- [ ] The default daemon's web UI is at `http://localhost:17333` across restarts, and tests and scratch daemons are unaffected (tests 8, 17).
- [ ] With a passkey, the port is held whenever the daemon runs; a taken port is reported and never replaced (tests 9, 10).
- [ ] A squatter's worker, cached page and cached asset do not survive onto Reef's page (tests 11, 12, 18).
- [ ] The verified origin shows in `reef evidence show`, `reef policy eval`, `reef identity passkey list`, the web pages and the snippet (tests 13, 15).
- [ ] A pin can hold `https://<dns name>[:port]` (test 1).
- [ ] ADR 0094, S-274 to S-278, the amended rows and documents; API revision 47; no migration; `just ci`, `just doc`, `web` lint, typecheck, unit tests and e2e pass.

## Verification

```sh
(cd web && npm ci && npm run lint && npm run typecheck && npm test && npm run build)
cargo nextest run -p reef-policy -p reef-evidence -p reef-web -p reef-api
cargo nextest run -p reef-daemon --test passkeys --test web_ui --no-fail-fast
cargo nextest run -p reef-daemon --lib config
cargo nextest run -p reef-vcs --test cli --test schema --test verified
just web-e2e && just doc && just preflight
```

By hand, with a real authenticator, on a daemon from this branch that serves `just web-build`'s assets:

1. Start it in a scratch state directory with `[ui] listen = "127.0.0.1:17334"`. Add a passkey, copy the snippet, commit the policy to a scratch repository's `main`, and Accept a proposal. It lands, and the address bar says 17334.
2. Restart the daemon. Accept another. It lands with the same pin. `reef daemon status` showed `BOUND` before `reef ui` ran.
3. Serve a page on another port that asks the same passkey to sign a pending approval's challenge. Touch it. Check that Reef refuses the assertion, naming both origins.
4. Change the port to 17335, restart, sign off. `reef evidence show` says the origin is not pinned, and Accept is refused with `pinned_origin`.
5. Stop the daemon. Run `python3 -m http.server 17334 --bind ::`. Start the daemon: the log and `reef daemon status` say the port is held, and `reef ui` fails with the new hint.
6. Open the desktop app on the fixed-port daemon, and use Continue in your browser for a sign-off.
7. In Safari and Chrome, repeat test 18 by hand.

## Alternatives, and why not

- **A port chosen once per state directory and remembered.** No two daemons collide, and no daemon needs a setting. But each machine gets its own pin, nobody can know the address by heart, and it hides nothing: any local user can list listening ports. Decision 1.
- **A separate origin for ceremonies only**, with the app on a random port as today. Reads and sessions would stay out of a squatter's reach. Every Accept would open a second window, since a cross-origin frame sets `crossOrigin` and is refused (`webauthn/mod.rs`, `client_data`). It is the better isolation and the worse daily experience; the leftovers work costs less. Kept as a follow-up if S-275 matters more later.
- **`<random>.localhost` as the host.** It names Reef in the platform's prompt and separates storage, and any local server can still answer for it at another port. It changes the RP ID, so every passkey registers again, and WebKit's resolution of `*.localhost` was not verified.
- **HTTPS on loopback.** A CA the machine trusts needs an install step, and its key is readable by the same uid. Browsers refuse WebAuthn on certificate errors.
- **A privileged port, launchd or systemd socket activation.** A service manager could hold the port while the daemon is down, which narrows S-275. The same uid can unload it, so it does not help S-274, and Reef's daemon is not a service today. Follow-up.
- **The origin in the challenge, the certificate predicate or a column.** `clientDataJSON` carries it, signed twice. A copy could disagree.
- **`certify` refusing other origins.** The attacker signs with its own code.
- **One origin for the whole `[passkeys]` table.** A key named `origin` is a legal identity name, and the phone needs a second origin beside the first.
- **A fingerprint and an origin in one string.** It needs its own grammar; TOML has tables.
- **A bare fingerprint that means "any localhost origin", with a warning.** It keeps the gap for whoever ignores the warning, to spare users who do not exist.
- **Falling back to a random port when the fixed one is taken.** It makes the address bar meaningless.
- **A text the authenticator shows.** WebAuthn has no transaction text that browsers ship; Secure Payment Confirmation shows a payment and nothing else.
- **`Clear-Site-Data`.** Measured: it does not evict the cached asset in Chromium before the page runs it.

## Out of scope

- Passkeys over the remote listener and the phone (R3.40). Only the pin's grammar and the RP ID's source are prepared.
- Any defence against a same-uid process that controls the browser or the daemon binary.
- Proving the first pin.
- Authenticator attestation (ADR 0085).
- Moving the session token out of `localStorage`.

## Risks

- **The port is taken on a user's machine by another program.** The web UI then needs one setting. Mitigated by a port in an unassigned block and a hint that says what to do.
- **The asset prefix breaks a lazy chunk or a stylesheet preload.** Vite's `base: "./"` has not been tried here. Test 12 and the e2e open every route. If it does not work, leave the asset part to a follow-up and say so in S-276.
- **The worker's CSP.** The answer carries `default-src 'none'`; the spikes had no CSP. Test 18 covers it in two engines, and Firefox is CI-only.
- **The hold surprises someone who never opens the UI.** They have a passkey, so they have opened it.
- **The probe on Linux** differs from macOS and was not measured; test 10 decides.
- **Ending sessions on `Service-Worker`** lets any local process end web sessions. Denial of service by a local user is not a boundary (threat model, "Loopback listener").

## What later tickets inherit

- **R3.40**: a pin may name `https://<dns name>[:port]`; the RP ID is the origin's host; the remote channel supplies its own origin where the web channel supplies `WebUi::origin`.
- **R4.13** (the hosted web UI): the same pin, one origin.
- **The release notes**: policies written against an unreleased `main` must add origins.

## Follow-ups to file

1. Hold the port through launchd or systemd while the daemon is down (S-275).
2. Keep the web session's token where another page at the origin cannot read it (S-275).
3. The worker, cache and asset spikes in Firefox on Linux, if test 18 cannot run there.
4. A separate ceremony origin, if the hand-off ever becomes the only place a touch happens (the desktop app already does it).

## Decisions for the user (each recommendation accepted, 2026-11-22)

1. **Which fixed port does Reef use?**
   - *Recommended: one well-known default, 17333, in the default state directory; port 0 elsewhere.* One address to know, one pin across a person's machines, no setup, and tests and scratch daemons change nothing. Cost: a second daemon or a second user on the machine sets a port, an SSH forward between two default machines needs a setting, and every site knows the port.
   - *A random port chosen once per state directory and remembered.* Nothing collides. Cost: every machine needs its own pin, and the address is not one a person can recognise.
   - *No default; the user sets `[ui] listen` before pinning.* No change for anyone who does not pin. Cost: a setup step in front of the first passkey, and the Passkeys page cannot offer a snippet until it is done.

2. **Does the daemon hold the port before the first `reef ui`?** ADR 0047 decided it does not.
   - *Recommended: yes, when the port is fixed and a passkey is registered.* It closes the window between daemon start and the first `reef ui`, when another process could hold the pinned origin with the daemon running. Nobody without a passkey gets a TCP port they did not ask for. Cost: ADR 0047 decision 2 gains an exception.
   - *Always, with a fixed port.* Simpler to state. Cost: every Reef user gets a listening port.
   - *Never, as today.* Cost: the window stays open on every daemon start.

3. **Do the browser leftovers (S-276) belong in this ticket?**
   - *Recommended: yes.* The fixed port is what makes them matter, and a brief squat would otherwise put a script on Reef's own page for good. About 250 lines and test 18.
   - *A follow-up before the release.* A smaller PR. Cost: `main`, and the dogfood daemon, carry a regression from the random port until it lands.
   - *Documented only.* Cost: the pin's promise depends on the port never having been taken once.
