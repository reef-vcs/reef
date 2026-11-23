# 0095. Paired devices

- Status: accepted
- Date: 2026-11-23
- Ticket: R3.40 ([brief](../dev-tickets/phase-3/R3.40-phone-web-app.md)), pull request 1. Pull request 2 adds passkeys at the remote origin and what a device decides.
- Amends: [0029](0029-daemon-api-v1.md) (decision 12's exceptions gain `RedeemPairing`, whose token is an access record), [0005](0005-tauri-desktop-shell.md) (the phone's app is the web app, not the shell), [0047](0047-web-ui-transport-and-local-listener.md) (the security headers refuse service workers, the local listener serves nothing an app is installed from, and live streams that fall silent are opened again, from another tab when the browser suspended the one that held them), [0049](0049-remote-listener.md) (decision 5: a third scope; decision 8: the gate takes a pairing code for one method; decision 11: a backoff spares live tokens on open connections)

## Context

Phase 3 ends when a person on a phone is told what needs them, opens the proposal and approves it. The remote listener already serves the web UI to a token. Three things stop a phone from using it well. A Home Screen app has its own storage, so it opens with no token and nothing in it can get one. The two scopes fit a phone badly: `read` cannot become anything else, and `write` edits files, submits jobs and asks for landings from anywhere. The threat model asks for the first scope a phone would want: "a phone that approves proposals is not a client that can `Create` a workspace".

The user decided that the phone's app is a Home Screen web app served by the remote listener, with no mobile shell, no developer account and no relay.

## Decision

1. **A device is a token of a third kind.** A row of `remote_tokens` with scope `read` and a `device_origin` (migration 0032). Code that does not know the column reads a `read` token, and that is the safer reading. A `write` row with a `device_origin` does not load. A device is linked to one person, lives 90 days by default and 366 at most, and is listed and revoked with the other tokens. It is not rotated (`DEVICE_TOKEN_ROTATE`): nobody could put the new secret on the phone.
2. **A device's call is a page's call.** It may call every `ACCESS_READ` method, and of the writes, only those on the web UI's allowlist. `remote/rules.rs` holds a second table, `DEVICE`, with a row for each of them, and a test fails when the two differ. In this pull request one row is open, `MarkInboxSeen`. The others wait until a device can show that a person is behind the call.
3. **One address.** `[remote] origin` names the address a phone opens. It is an `https` origin whose host is in `[remote] names`, in the form a policy's pin takes ([0094](0094-the-pinned-web-origin.md)). A host under `reef.localhost` is refused, because the local web UI's passkeys are there. A device's token works at the `device_origin` it was paired at, while that is still `[remote] origin`; anything else is `REMOTE_ORIGIN_CHANGED`. The check is on `Host`, so it stops a page at another of the daemon's names. A program that holds the token can send any `Host`.
4. **Pairing is a code typed once.** `RemoteService.StartPairing` is ADMIN, so it runs on the socket alone. It draws 8 characters of Crockford's base32 (40 bits), keeps their digest in memory, 4 at once at most, for 10 minutes. `reef remote pair` prints the address and the code. The QR code holds the address alone: a camera opens it in Safari, and Safari's storage is not the Home Screen app's, so a code in it would pair the wrong place. The phone sends the code as `authorization: Bearer reef_pair_<code>` to `RedeemPairing`, the one method the gate admits it for. No token calls that method, so it stays true that no RPC is served without a credential. The code is taken before the token is made, so a replay finds nothing.
5. **Guessing is bounded twice.** A wrong code counts against its address as a wrong token does (10 in a minute, then a backoff that doubles). 100 wrong codes, from any addresses, end every code waiting. A wrong, used, expired or misdirected code gets one answer. A code sent to another address than `[remote] origin` is refused before it is looked at: it is not used up there, and it is not counted as a guess at a code. An address in backoff keeps its live tokens on the connections it opened: before, anyone who shared the address could have ended a paired phone's page with ten wrong codes, and typed codes make those cheap.
6. **The app.** A web manifest, three icons and `viewport-fit=cover`. The remote listener serves them, and the local listener answers `404`: an app installed at a run's name would be dead at the next start. Both listeners send `worker-src 'none'`. Until now `script-src 'self'` let a page at either origin register a same-origin service worker. Pull request 3 ships a worker and opens the remote listener's.
7. **The page knows that it is a phone's**, from the answer to its pairing, and offers no write. What it knows decides nothing: the gate does. It keeps its token when a link brings another, since any site can open such a link and a pairing cannot be opened again.

## Consequences

- A lost phone reads every repository, its diffs and its inbox until `reef remote token revoke` or the 90 days end. It changes nothing but its person's inbox marks, and the snapshots a read may take (a read that names `@` records what the workspace holds, with the device's token on the operation).
- A changed `[remote] origin` stops every device, and each pairs again at the new one. Its page says where that is and keeps its token, so an `origin` missing once, or put back, unpairs nobody. A port forward works while the phone's address is the one in `[remote] origin`.
- A phone must trust the certificate, so the generated one does not do. [docs/phone.md](../phone.md) gives three ways.
- A connection that dies silently (a phone asleep, or on another network) is cut by the page: every live stream has a frame at least every 15 s, and one silent for 40 s, or for 20 s when the page is shown or the network is back, is opened again with the others (`web/src/live/silence.ts`). It applies to every page, on either listener.
- A restart of the daemon drops the codes waiting.
- `StartPairing`'s answer and the terminal `reef remote pair` runs in hold a credential for 10 minutes.

## Alternatives

- **A fifth value in the `scope` column.** The `CHECK` would need the table rebuilt, and code that does not know the value fails instead of reading `read`.
- **A `write` token on the phone.** It works today, and edits files and runs jobs from anywhere.
- **Passkey sign-in instead of a device token.** Discoverable passkeys would store no secret. The passkey has to be registered first, which needs pairing anyway.
- **A code in the QR, carried by a dynamic `start_url`.** Nothing to type, but the secret sits in a URL that Safari keeps.
- **An RPC that tells a device its own name and end.** The pairing's answer already holds both.
- **A device that revokes itself.** It would be a write a stolen token has, and the person who unpairs on the phone already removes the only copy of the secret.
