# 0094. The pinned web origin

- Status: accepted
- Date: 2026-11-22
- Ticket: #346 ([brief](../dev-tickets/phase-3/issue-346-pin-the-web-origin.md))
- Amends: [0047](0047-web-ui-transport-and-local-listener.md) (decision 2: the listener is bound at start when its port is fixed and a passkey is registered; decision 4: with a fixed port the launch file has a script and clears the address first; the rejected alternatives "Binding at start" and "A fixed default port"), [0061](0061-repository-policy-file.md) (`[passkeys]` holds pins with an origin; the input's key gains `passkey_origin`), [0085](0085-verified-human-presence.md) (decision 2: a pin names the origin too; its look-alike consequence), [0088](0088-triage-inbox.md) (one of the reasons against browser storage: in the default state directory the origin now outlives a restart)

## Context

A passkey's RP ID is `localhost`, so a browser lets a page at any `http://localhost:<port>` ask for its assertion, and signs that page's origin into it. The daemon accepts an assertion only from its own origin. A certificate's reader could not check the port, because the port changed at every start. Any process of the same user can read the authority's key (S-58), so it could show a look-alike approval page on another port, take one touch over a challenge it derived, and certify the result itself. The sign-off counted everywhere (S-255).

No release contains policy or passkeys, so there is no written policy to keep working.

A port that does not change brings its own problem, which spikes in Chromium and WebKit showed. A process that holds the port while the daemon is down leaves things in the browser that a random port made harmless at the next start: a service worker, a cached page, and cached scripts at the app's hashed paths, which Reef's own fresh page then runs. `Clear-Site-Data` does not evict the cached script in Chromium before the page runs it. On macOS, binding the port under a process that listens on every address succeeds, so the bind does not see it.

## Decision

1. **A pin is a fingerprint and an origin, both required.** `[passkeys]` maps an identity to a list of `{ fingerprint, origin }`. A key's presence counts as `user_verified` only when one pin of its identity has both its passkey and its certificate's origin, byte for byte. `Policy::presence` is the only comparison, so attestor sets, `[[signoff]]`, `[owners]`, CEL and web landings follow. The format version stays 1: no release reads either form, and a bare fingerprint is a load error that says what to write.
2. **The origin is read from the certificate, never stored beside it.** `clientDataJSON` holds it, signed by the passkey and by the authority. `certify` and `check_certificate` do not check it against a pin: they hold no policy, and an attacker with the authority's key does not call them.
3. **The default port is 17333 in the default state directory, and the kernel's in any other.** One address to know, and one pin for a passkey synced between machines. Tests and scratch daemons never contend for it. A port another process holds is an error and never replaced by another: a fallback would teach the person that Reef's port varies.
4. **The daemon holds a fixed port from its start once a live passkey exists.** A daemon whose user never adds a passkey and never runs `reef ui` still opens no TCP port, which is the promise of 0047's decision 2 that matters. Before a fixed port is bound, the daemon connects to it on both families, and an answer is the same error as a failed bind. A failed hold is tried again until the port is the daemon's.
5. **A launch file opens the address only once the browser was told to clear it.** With a fixed port, the launch file loads `/reset/<id>` from the address as an image, once for each launch file. The file is another origin, which no process can have prepared, so no worker of the address intercepts the request. The answer's `Clear-Site-Data: "cache", "storage"` removes the address's workers, caches and stored session. The file goes to the session's URL when the image loads. Otherwise it opens nothing and says to clear the browser's data, and it waits without a limit, since a large cache takes seconds. Chromium honours this (measured against workers never checked for updates, many scopes, and pages cached for every query); Playwright's WebKit ignores it. The file has a script then, which 0047's did not.
6. **What WebKit keeps is ended piecemeal.** A request for a service worker's script is answered with a worker that unregisters itself, and every web session ends with its streams. Reef's page removes every worker and cache of its origin, on `localhost` only: the remote listener's app will have a worker of its own (R3.40). The hashed assets are served only under a prefix of the daemon's run, and launch URLs carry an id. A session's token stays in its tab until the daemon accepts it, since any page can now link to the port with a fragment of its own.
7. **A pin can name `https://<dns name>[:<port>]`**, whose host is the RP ID, for passkeys through the remote listener (R3.40). Nothing remote is built here.

## Alternatives

- **A random port remembered per state directory.** Nothing collides, but every machine needs its own pin, nobody can know the address by heart, and any local user can list listening ports.
- **A separate origin for ceremonies only**, with the app on a random port. It isolates sessions better, and every Accept would open a second window, since a cross-origin frame's assertion is refused.
- **`<random>.localhost`, or HTTPS on loopback.** The first changes the RP ID and any local server still answers for it at another port. The second needs a CA the machine trusts, whose key the same uid can read.
- **Cleaning up from inside the pinned address only.** The first form of decision 5: a fresh query on the launch URL, per-run asset paths and the worker eviction. Review broke it in Chromium: a worker registered with `updateViaCache: "all"` never asks the server for its script, `No-Vary-Search` makes a cached page answer every query, and each launch spends only one of many registrations.
- **A launch page on a second loopback listener, at a port the kernel picks.** The second form of decision 5. macOS hands such ports out in sequence, and a page at the pinned port can register workers at thousands of them, so the page itself could be poisoned; and it went on when the clear failed, which enough cookies or 64 idle connections arrange.
- **A bare fingerprint that means any origin, with a warning.** It keeps the gap, for users who do not exist.
- **The origin in the challenge, the predicate or a column.** A copy that could disagree with `clientDataJSON`.

## Consequences

- A look-alike page on any port but the pinned one can get a touch, and its sign-off counts for nothing. Its trace is a warning in the identity audit log and `VERIFIED_SIGNOFF_REQUIRED` with `missing` `pinned_origin`.
- **Not closed.** Code that runs as the person can stop the daemon and serve the pinned port, control the browser, or replace the daemon (S-274); one uid is not a boundary (ADR 0027). An `https` origin served outside that uid closes it. The first pin is still the person's check: a look-alike's snippet can name the right origin and an agent's fingerprint (S-250). A squatter's script at the origin, in a tab left open or kept in the browser, can read the next session's token, and with it make the allowlisted writes and get one misled touch certified by the daemon itself (S-275). In WebKit a squatter with many workers answers some launches until Reef's page loads once, a printed URL, a bookmark or a reload is not cleared first in any browser, and cookies set by any `localhost` page can keep the UI from opening (S-276): after another process held the port, clear the browser's data for the address.
- Each launch from `reef ui` drops what the browser stored for the address: other tabs keep the session they hold in memory, and a dev-mode choice is forgotten.
- A second daemon or a second user on one machine sets a port of its own. An SSH forward between two machines on the default port needs `[ui] listen` on one of them.
- Moving the port stops earlier sign-offs on open proposals from counting until the policy pins the new origin. Landed changes are not judged again.
