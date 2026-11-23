# 0094. The pinned web origin

- Status: accepted
- Date: 2026-11-23
- Ticket: #346 ([brief](../dev-tickets/phase-3/issue-346-pin-the-web-origin.md))
- Amends: [0047](0047-web-ui-transport-and-local-listener.md) (decision 2: the listener is bound at start when its port is fixed and a passkey is registered; the rejected alternatives "Binding at start" and "A fixed default port"), [0061](0061-repository-policy-file.md) (`[passkeys]` holds pins with an origin; the input's key gains `passkey_origin`), [0085](0085-verified-human-presence.md) (the RP ID is `reef.localhost`, and the listener serves at a name of its run under it; decision 2: a pin names the origin too; its look-alike consequence)

## Context

A passkey's RP ID was `localhost`, so a browser let a page at any `http://localhost:<port>` ask for its assertion, and signs that page's origin into it. The daemon accepts an assertion only from its own origin. A certificate's reader could not check the port, because the port changed at every start. Any process of the same user can read the authority's key (S-58), so it could show a look-alike approval page on another port, take one touch over a challenge it derived, and certify the result itself. The sign-off counted everywhere (S-255).

No release contains policy or passkeys, so there is no written policy or registered passkey to keep working.

A pin needs an address that outlives the daemon's run, and a fixed address brings its own problem. A process that holds the port while the daemon is down leaves things in the browser that a new port made harmless: service workers that answer every navigation before the daemon sees a request, cached pages and scripts, and storage. This ticket first kept `http://localhost:<fixed port>` and cleaned up, three times: with fresh launch URLs, per-run asset paths and an answer that unregisters a worker; with `Clear-Site-Data` asked for by a page at another port; and asked for by the launch file. Review broke each with a running attack. A worker registered with `updateViaCache: "all"` never asks for its script, and `No-Vary-Search` makes a cached page answer every query. macOS hands out kernel-chosen ports in sequence, so the other page could be prepared. Chrome 157 and Firefox 157 limit `Clear-Site-Data` on a cross-site response to the asker's partition. The three shared a premise: that an address others could reach can be made clean.

## Decision

1. **A pin is a fingerprint and an origin, both required.** `[passkeys]` maps an identity to a list of `{ fingerprint, origin }`. A key's presence counts as `user_verified` only when one pin of its identity has its passkey and covers its certificate's origin. `Policy::presence` is the only comparison, so attestor sets, `[[signoff]]`, `[owners]`, CEL and web landings follow. The format version stays 1: no release reads either form, and a bare fingerprint is a load error that says what to write.
2. **Each bind of the listener, so each run of the daemon, serves at a name of its own**, `http://<16 random hex>.reef.localhost:<port>`. Browsers resolve names under `localhost` to loopback. The name is drawn when the port is bound and told to no one before, so no process that held the port can have left anything in the browser at it, and a page at any other name is another origin, so a squatter's tab shares no storage with Reef's. The gate accepts only this run's name.
3. **The RP ID is `reef.localhost`**, which every run's name is under, so a passkey outlives the run. A page at `localhost` or any other name cannot ask for it at all, and the platform's prompt says "reef.localhost". A certificate's reader accepts an origin whose host is the RP ID or a name under it.
4. **A local pin names `http://reef.localhost:<port>`** and covers `http://<16 hex>.reef.localhost:<port>` and nothing else. An `https` pin is one address, byte for byte, whose host is the RP ID (R3.40).
5. **The origin is read from the certificate, never stored beside it.** `clientDataJSON` holds it, signed by the passkey and by the authority. `certify` and `check_certificate` do not check it against a pin: they hold no policy, and an attacker with the authority's key does not call them.
6. **The default port is 17333 in the default state directory, and the kernel's in any other.** Tests and scratch daemons never contend for it. A port another process holds is an error and never replaced by another.
7. **The daemon holds a fixed port from its start once a live passkey exists**, and tries again until it has it. A daemon whose user never adds a passkey and never runs `reef ui` still opens no TCP port, which is the promise of 0047's decision 2 that matters. Before a fixed port is bound, the daemon connects to it on both families, and an answer is the same error as a failed bind: on macOS a bind succeeds under a process that listens on every address.

## Alternatives

- **`http://localhost:<fixed port>` with cleanup.** A stable address to bookmark, and the three failures above.
- **`<random>.localhost` with the RP ID `localhost`.** Chromium and Chrome refuse an RP ID of one label from a name under it (measured).
- **A random port remembered per state directory.** Every machine needs its own pin, any local user can list listening ports, and the browser problem is the same.
- **A separate origin for ceremonies only.** Every Accept would open a second window, at an address with the same problem.
- **HTTPS on loopback.** It needs a CA the machine trusts, whose key the same uid can read.
- **A bare fingerprint that means any origin, with a warning.** It keeps the gap, for users who do not exist.
- **The origin in the challenge, the predicate or a column.** A copy that could disagree with `clientDataJSON`.

## Consequences

- A look-alike page on any other port can get a touch, and its sign-off counts for nothing. Its trace is a warning in the identity audit log and `VERIFIED_SIGNOFF_REQUIRED` with `missing` `pinned_origin`.
- The address of the web UI changes at every start, as with a kernel-chosen port, so it cannot be bookmarked, and what the app keeps in the browser ends with the run. A tab of an earlier run on the same port is told that its session ended.
- **Not closed.** Code that runs as the person can stop the daemon and serve a page under the pinned origin at the same port, control the browser, or replace the daemon (S-274); one uid is not a boundary (ADR 0027). An `https` origin served outside that uid closes it. A tab such a page left open can ask for a touch that counts if it can certify it. The first pin is still the person's check: a look-alike's snippet can name the right origin and an agent's fingerprint (S-250). A page under `reef.localhost` can set cookies that reach every run's name, enough of them to keep the UI from opening until the person clears them (S-276).
- Measured in Chromium 156, Chrome 157 and Playwright's WebKit, and in Firefox by CI. Passkeys at `reef.localhost` in Safari, with a real authenticator, are a manual check; Playwright's WebKit accepts the RP ID and has no virtual authenticator.
- A browser that does not resolve names under `localhost` cannot open the UI. Chromium and Firefox do it themselves; Safari uses the system resolver, which does on macOS; glibc with no `resolve` or `myhostname` module may ask DNS.
- **A system HTTP proxy on macOS takes these names** unless `*.localhost` is on its bypass list, where `localhost` was always excluded (measured in CFNetwork, not in Safari end to end). In Safari and the desktop app the UI then does not open, and a tab open when a proxy is pushed sends its requests, the session's token in them, to the proxy in plain text (S-277). Chrome 157 and Firefox do not.
- App Transport Security refuses `http` to a name under `localhost`, so the desktop app's `Info.plist` excepts `reef.localhost`, which `scripts/shell-build.sh` checks.
- A second daemon or a second user on one machine sets a port of its own. An SSH forward between two machines on the default port needs `[ui] listen` on one of them.
- Moving the port stops earlier sign-offs on open proposals from counting until the policy pins the new origin. Landed changes are not judged again.
