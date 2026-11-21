# Identities and session keys

An evidence object says which key signed it. An **operator identity** says whom that key answers to: a person, or a CI pipeline. The daemon certifies short-lived **session keys** for identities, and every evidence read reports the identity its key is bound to. [ADR 0057](adr/0057-operator-identities-and-session-keys.md) records why; [docs/evidence.md](evidence.md) has the evidence format, and [docs/provenance.md](provenance.md) the provenance records the daemon signs with keys it holds for each workspace's session.

A certificate says which identity a key is bound to and what the requester declared it for. It never says that the identity should be trusted: a repository's [policy](policy.md) decides that, with attestor sets of identities, actors and authorities. A person's **passkey** can verify that the person, not an agent running as them, approved a sign-off ([Passkeys and verified presence](#passkeys-and-verified-presence)), and a policy counts that only for the passkeys it pins.

## Identities

An identity is `person` or `ci`, with a name (`[a-z0-9][a-z0-9._-]{0,62}`, not starting with `idn_` or `ses_`, which mark ids) that is never reused in the daemon's database, even after the identity is revoked, so a policy that names `alice` cannot be satisfied by a newcomer. Restoring an older backup can bring a name back with a new id, so a policy that must not be fooled pins the identity's id and the authority's key id, not the name alone. Agents are not identities: an agent acts for an identity, and a key records it as the declared `actor` (`agent`, `executor`, or `human` for the person signing directly) and its label as `agent`. An executor taking jobs from Reef's queue issues one key per job with actor `executor` ([docs/executor-protocol.md](executor-protocol.md)); through the remote listener it needs a token linked to an identity.

```sh
reef identity list                       # the daemon creates one person identity, named after you
reef identity create ci-main --kind ci   # a pipeline
reef identity show ci-main
reef identity revoke ci-main --reason "pipeline retired"
```

At its first start, the daemon creates a `person` identity named after the login user, lowercased (`operator` when the name has nothing usable), and uses it when a request on the socket names none. `[session_keys] default_identity` in the daemon's `config.toml` names another live person identity; one that names no live person identity is logged at start, and requests for the default are refused (`DEFAULT_IDENTITY_UNAVAILABLE`) until it is fixed. The default cannot be revoked while it is the default: name another first. Only the daemon's user creates and revokes identities; a remote caller cannot. `[identity]` in the configuration is unrelated: it names the daemon as the author of commits it writes.

## The authority

The daemon's issuing authority is an ed25519 key at `<state>/identity/authority.pem` (`0600`), made at its first start. Its public key and key id are kept in `reef.sqlite3` for good, so a certificate it issued verifies after the key is retired. `reef identity authority show` prints the current key id and public key, which another party pins, then the retired ones.

The key file is never in a backup. A restore, or a missing or replaced file, retires the authority, and the next start makes a new one; certificates the old one issued still verify. `reef identity authority rotate` makes a new one on demand; `--revoke` also revokes the old one, from its creation unless `--since`, so everything it issued reads `revoked`. `reef identity authority revoke <key id>` revokes an authority already retired, by a restore, a replaced key file, or a rotation without `--revoke` (`--since`, `--since-created`); the current one is rotated instead.

## Session keys

A session key is generated in the process that signs with it. The daemon sees only its public key and a proof that the requester holds it, certifies it, and stores the certificate; no private session key is stored anywhere. Keys live an hour by default (`[session_keys] lifetime`, at most `max_lifetime`, 24 hours by default, at most 7 days; neither under 5 minutes). Renewal is a new key with `--session <id>`: the same identity and actor, through the same channel (the socket, or the same remote token), a new window.

Who gets a key for what:

| Caller | A `person` identity | A `ci` identity | Declared `human` |
| --- | --- | --- | --- |
| The socket | any live one; none named is the default | never | yes, with `presence: none` |
| The web UI, through an approval a passkey resolved (R3.43) | any live one | never | always, with `presence: user_verified` |
| A remote token linked to identity X | X only | X only | never |
| A remote token linked to none | none | none | never |
| A keyless bundle (R3.42), through the socket or any `write` token | the identity whose OIDC match accepts the certificate | the same | person only |

Link a token when you create it: `reef remote token create ci --scope write --identity ci-main`. That is a `ci` identity's only way to get keys, so a local agent cannot mint "CI" evidence with one call.

### Getting a key

- `reef evidence add` issues one for five minutes, signs the one envelope, and drops it. `--identity` and `--actor` choose what it is certified for; `--key` signs with a key file instead.
- `reef evidence add --actor human --verified` gets a `user_verified` key through an approval that you resolve with your passkey in the browser ([Approving a sign-off](#approving-a-sign-off)).
- In Rust, `reef_client::Client::issue_session_key(identity, actor, lifetime, session)` returns a `SessionKey` whose `signing_key` signs with `reef_evidence::Claim::sign`.
- From another language, `reef identity session issue -o json` prints `key`, `private_key` (PKCS#8 PEM) and `certificate`. The private key is a credential: it is printed once, only as JSON (the text form refuses before issuing), and Reef keeps it nowhere.

Over the API, `IdentityService.IssueSessionKey` takes the key's DER `SubjectPublicKeyInfo`, the identity (empty for the default), the actor, an optional lifetime and session, and a proof: the session key's ed25519 signature over the DSSE pre-authentication encoding of payload type `application/vnd.reef-vcs.session-key-proof` and this payload, the current authority's key id first (`ListAuthorities`, the one marked current), the identity and session exactly as sent (empty for none), the actor lowercase:

```text
reef-session-key-proof/v1\n<authority key id>\n<key id>\n<identity>\n<actor>\n<session>\n
```

The proof binds its audience, the identity, the actor and the session, so nobody can bind a key they do not hold, and a proof cannot be replayed for another binding or at another daemon. A refusal of the proof (`INVALID_SESSION_KEY_REQUEST`, `field` `proof`) carries the current `authority_key_id`: after a rotation, sign again for that one. A key that is already certified, or that already signed evidence as a bare key, is refused: a session key is always fresh.

### The certificate

A certificate is an in-toto Statement v1 in the same DSSE envelope as evidence, signed once by the authority:

```json
{"_type":"https://in-toto.io/Statement/v1",
 "subject":[{"name":"session-key","digest":{"sha256":"<the session key's key id>"}}],
 "predicateType":"urn:reef-vcs:session-key:v1",
 "predicate":{"publicKey":"MCowBQYDK2VwAyEA…","identity":{"id":"idn_8f3a0c2d91e4b675","name":"alice","kind":"person"},
              "actor":"agent","agent":"builder-3","session":"ses_41c9e07ab2d35f18","presence":"none","custody":"client",
              "notBefore":"2026-11-06T10:00:00.000Z","notAfter":"2026-11-06T11:00:00.000Z",
              "issuer":{"kind":"reef-daemon","keyId":"<the authority's key id>","host":"mbp.local"}}}
```

`reef schema evidence session_key_predicate` prints its JSON Schema. A verifier checks it as evidence is checked (the envelope, the authority found by `keyid`, the signature, then the statement) and refuses a `presence`, `custody`, `actor`, kind or issuer it does not know; `reef_evidence::session::check_certificate` does that in Rust. A `user_verified` certificate also carries `presenceEvidence`, which the verifier checks too ([The certificate's evidence](#the-certificates-evidence)).

## Keyless (Sigstore)

A **keyless** session key is certified by Sigstore's Fulcio instead of the daemon, against an OIDC identity: a CI job's own token, or a token a person supplies. It is how a CI pipeline's evidence comes to be bound to its `ci` identity without a long-lived secret: the binding rests on what the CI provider's OIDC issuer says about the job, not on who holds a Reef token. [ADR 0066](adr/0066-sigstore-keyless-session-keys.md) records why.

How it works:

1. The signing process generates an ed25519 key and gets an OIDC token for audience `sigstore`.
2. Fulcio certifies the key for the token's issuer and subject, for 10 minutes.
3. The key signs a **binding statement** about itself, and Rekor logs it in public.
4. The process registers the Sigstore bundle (the certificate, the log entry and the statement) with the daemon, which verifies it offline against its trusted root and binds the key to the identity whose OIDC match accepts the certificate.
5. The key signs evidence like any session key, within Fulcio's window.

### Which identity: OIDC matches

An identity accepts keyless keys for exact OIDC issuers and subjects, compared byte for byte with the certificate's issuer extension and its SAN. One live match per issuer and subject across all identities, so a certificate binds to one identity or none.

```sh
reef identity create ci-main --kind ci \
  --oidc https://token.actions.githubusercontent.com=https://github.com/OWNER/REPO/.github/workflows/reef.yml@refs/heads/main
reef identity oidc add alice --issuer https://accounts.google.com --subject alice@example.com
reef identity show ci-main                         # lists its matches and their ids
reef identity oidc revoke oidc_41c9e07ab2d35f18 --since-created
```

- **GitHub Actions**: issuer `https://token.actions.githubusercontent.com`, subject the workflow file at a ref, `https://github.com/OWNER/REPO/.github/workflows/WORKFLOW.yml@refs/heads/main`. Pin the ref to `main` (or a protected branch): a match on another ref lets that ref's workflow code mint the identity's keys, and so does a workflow triggered by `pull_request_target`. There are no patterns, on purpose; a second workflow is a second match.
- **GitLab CI**: issuer `https://gitlab.com` (or your instance), subject `https://gitlab.com/GROUP/PROJECT//.gitlab-ci.yml@refs/heads/main`.
- **A person**: the issuer their login went through (`https://accounts.google.com`, `https://github.com/login/oauth`, `https://login.microsoftonline.com`) and their email.

Revoking a match applies to every key bound through it, with the same `invalid_from` rules as any revocation. Adding a match never rebinds a key registered before it.

### Getting a keyless key

```sh
reef identity session issue --keyless --actor executor -o json
reef evidence add --kind test --result pass --keyless --actor executor
```

The OIDC token comes from the first of these that applies; nothing opens a browser or waits for a person:

| Source | How |
| --- | --- |
| `--oidc-token-file PATH` | the token in a file, mode `0600` or tighter |
| `SIGSTORE_ID_TOKEN` | the token itself: GitLab's `id_tokens:`, or any CI or script |
| GitHub Actions | `ACTIONS_ID_TOKEN_REQUEST_URL` and `_TOKEN`, with `permissions: id-token: write` |

A person gets a Sigstore-audience token from a tool they run, such as `gcloud auth print-identity-token --audiences=sigstore` for a Google account. Fulcio and Rekor are the public-good instances (`https://fulcio.sigstore.dev`, `https://rekor.sigstore.dev`); `REEF_SIGSTORE_FULCIO_URL` and `REEF_SIGSTORE_REKOR_URL` name others. In Rust, `reef_client::Client::issue_keyless_session_key`. The reference executor signs keyless with `[executor] keyless = true` ([docs/executor.md](executor.md)).

Registration (`IdentityService.RegisterSigstoreKey`) goes through the socket or any `write` remote token: the OIDC identity decides, not the channel, so a `ci` identity's keyless key is accepted over the socket. A key's window is its certificate's, 10 minutes on the public-good instance, judged at the time the daemon first witnessed the evidence; `human` is never bound to a `ci` identity, and `presence` is always `none`: an OIDC login says who logged in recently, not who is at the keyboard.

### The binding statement

```json
{"_type":"https://in-toto.io/Statement/v1",
 "subject":[{"name":"session-key","digest":{"sha256":"<the key's id>"}}],
 "predicateType":"urn:reef-vcs:sigstore-session-key:v1",
 "predicate":{"actor":"executor","agent":"reef-executor@ci-7","session":"ses_41c9e07ab2d35f18",
              "presence":"none","custody":"client"}}
```

It is the payload of a DSSE envelope (`application/vnd.in-toto+json`) signed by the certified key, logged in Rekor v1 as a `dsse` 0.0.1 entry, and carried in a Sigstore bundle v0.3 whose verification material is the Fulcio leaf. The session is the client's: a new `ses_` id for its first key, repeated by its renewals; a known session must be a keyless one with the same identity and actor. `reef schema evidence sigstore_binding_statement` prints its JSON Schema. Any Sigstore verifier checks the bundle (with the key's SPKI as the artifact whose SHA-256 is the subject); `reef_evidence::sigstore::check_sigstore_bundle` does that in Rust, behind the `sigstore` feature.

### The trusted root

The daemon verifies bundles offline, against the public-good `trusted_root.json` Reef ships, or the file `[sigstore] trusted_root` names (an absolute path; a private Sigstore, or a root newer than the build), read at start. It never fetches one. Every read verifies the stored bundle again, so a root without a key's CA or log makes that key read `invalid`. `[sigstore] enabled = false` refuses registration and reads every keyless key `invalid`. A root that does not load is logged, and registration is refused (`SIGSTORE_UNAVAILABLE`) until it is fixed.

The Fulcio CA a keyless key came from is recorded as an authority of kind `sigstore` that never issues: `reef identity authority show` lists it, policy can pin its key id, and `reef identity authority revoke <key id>` revokes everything it issued, here.

### What is public

The certificate (the workflow and repository, or the email) and the binding statement's hashes are in Rekor and in Fulcio's certificate transparency log, for a private repository too. Use a private Sigstore deployment through `[sigstore] trusted_root` and the endpoint variables if that matters.

## Passkeys and verified presence

On one machine, an agent running as you can do anything you can on the socket, including getting a key declared `human` for your identity. A passkey is the one thing it cannot use alone: every assertion needs you at the authenticator, with a touch and a fingerprint, a face or a PIN (WebAuthn's user verification). Reef uses your passkey to approve one sign-off at a time, and a repository's policy counts the approval only for passkeys it pins. [ADR 0085](adr/0085-verified-human-presence.md) records why.

Passkeys work in the local web UI only. WebAuthn needs a browser, and browsers refuse it at an IP-literal origin, so `reef ui` serves `http://localhost:<port>`, and the passkeys' RP ID is `localhost`. Passkeys through the remote listener are R3.40's.

### Adding and removing passkeys

```sh
reef identity passkey add              # open the web UI's Passkeys page
reef identity passkey add --print-url  # print the page's URL instead, for SSH
reef identity passkey list             # each passkey, and the [passkeys] line to pin them
reef identity passkey reset alice      # remove every passkey of alice's, on the socket only
```

The Passkeys page, linked from the web UI's Settings panel, lists each person identity with its passkeys: label, fingerprint, when it was added and when it was last used. **Add** registers a passkey on this browser's authenticator under a label you type, and **Remove** removes one.

- **The first passkey is trusted on first use.** Any web UI session can add an identity's first passkey. The daemon's registration challenge is single use, valid for 5 minutes, and only for the web UI session that started it.
- **Every later add, and every removal, needs an existing passkey.** The daemon refuses one without a user-verified assertion by one of the identity's live passkeys (`PASSKEY_REQUIRED`), so the page asks for a touch on an existing passkey first. Removing a passkey leaves the keys it certified as they are. An approval that is being checked when its passkey is removed or reset certifies no key: it is refused (`PASSKEY_ASSERTION_INVALID`, check `passkey`) and stays pending.
- **A reset is the way back.** `reef identity passkey reset IDENTITY` removes every live passkey of the identity and revokes every unexpired key any of its passkeys certified, removed ones included, from now, in one step. Use it when a passkey is lost, or when one is listed that you did not add. It runs on the socket only, and the daemon logs it as a warning. The next passkey is the first again.
- Registration and removal need a browser: on the socket they are `PASSKEY_NEEDS_WEB`, and through the remote listener `PASSKEY_OVER_REMOTE`.
- An identity holds at most 32 live passkeys. A label is 1 to 64 bytes. A passkey is ES256, EdDSA or RS256; any other algorithm is `PASSKEY_ALGORITHM`.

A passkey's **fingerprint** is the SHA-256 of its public key (DER `SubjectPublicKeyInfo`), in lowercase hex, as Reef's key ids are. `reef identity passkey list` prints each passkey's label, id, fingerprint, when and from which web UI session it was added, when it was last used and when it was removed, and then the `[passkeys]` line for the identity, with every live passkey. Every add, removal and reset is recorded with its time and web UI session, and logged under `reef_daemon::identity::audit`.

### Pinning a passkey

A passkey counts for nothing in a repository until that repository's policy pins it. Trust on first use alone is not enough: an agent running as you can open a `reef ui` session in a headless browser with a software authenticator and register its own passkey before you do. So pin yours in the destination's `reef-policy.toml`, by identity name or id:

```toml
[passkeys]
alice = ["3c9e0d5f6a7b8c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f"]
```

On the Passkeys page, **Confirm a passkey to pin** asks your authenticator for a touch, checks it against the key Reef holds, and **Copy policy snippet** copies the line for the passkeys it confirmed, with each fingerprint computed from the key the touch verified. This holds only on the daemon's own page: open it yourself, with `reef ui` or `reef identity passkey add` from your own terminal. A page on another `localhost` port can look the same and show an agent's fingerprint, and with the default `[ui] listen` port 0 the port changes at every start, so the address bar gives no cue (S-255). `reef identity passkey list` prints the line with every live passkey. A change to the policy file needs `[owners]`' sign-off; with `presence = "user_verified"` in `[owners]`, that takes an owner's pinned passkey, so an agent cannot change the pins. [docs/policy.md](policy.md#pinned-passkeys) has the rules.

The web UI lands a change only with a pinned passkey's sign-off: **Accept** on a proposal page lands on a destination whose policy pins the passkey you touch, and is refused there until one is pinned (`VERIFIED_SIGNOFF_REQUIRED`, `missing` `pin`), whatever else the policy asks ([proposals](proposals.md#reviewing-in-the-web-ui)). Pinning is how a destination takes landings from the web UI at all.

### Approving a sign-off

Each verified sign-off is its own approval: one fresh key, one touch, one statement about one change.

In the web UI, **Sign off** on a change's detail panel in the smartlog signs off the change as the page shows it: kind `signoff`, result `pass`, on its tree. It asks for your passkey, then adds the sign-off and lists it with `user_verified` and whether the repository's policy pins your passkey. It is disabled, with the reason, while your identity has no passkey.

From the CLI:

```sh
reef evidence add --kind signoff --result pass --actor human --verified -r @-
```

`--verified` needs `--actor human` and takes any kind, result and target. The command builds the statement over the revision's tree and a fresh key, and asks the daemon for an approval. It then opens the approval page, `/approve/<id>`, in your browser through a new web UI session, as `reef ui` opens the app; `--print-url` prints the URL instead, for SSH. It waits up to `--timeout` (default 2 minutes) for you to approve or decline. When you approve, it signs the statement with the key the approval certified and adds the sign-off, whose presence is `user_verified`. A decline, the approval's expiry, an approval the daemon no longer holds (a restart drops pending ones) or the timeout ends the command with exit code 5 and the reason. It asks nothing on the terminal and runs no login: the approval happens in the browser.

The approval page names, before its buttons, the identity you sign as, the kind, result and target, the change with its description and author, the repository, its tree (the sign-off covers the whole tree, changes below that have not landed included), who asked (a command, with the agent name and workspace it declares, or a web UI session), and the time left. **Approve** asks for your passkey; **Decline** ends the approval. The page approves nothing when it loads. Your platform's passkey prompt shows only "localhost", so the page is where you read what you approve.

Over the API, the CLI's steps are:

1. `IdentityService.RequestApproval`, on the socket or through the web UI, with the key's DER `SubjectPublicKeyInfo`, its proof of possession (as for `IssueSessionKey`, with actor `human` and an empty session), the in-toto statement it will sign, and the revision. The daemon parses the statement as `AddEvidence` does, checks that its subject is the revision's tree, and chooses a new session for the key. The approval waits 2 minutes in the daemon's memory. An identity has at most 8 pending and the daemon holds at most 256 (`TOO_MANY_APPROVALS`).
2. `IdentityService.GetApproval` with `wait` returns when the approval is approved, with the key's certificate, or declined, or expired. An ended approval stays readable for 2 minutes after its expiry; after that, or after a restart, it is `APPROVAL_NOT_FOUND`.
3. `IdentityService.ResolveApproval` approves with a passkey's assertion, through the web UI only (`APPROVAL_NEEDS_WEB` on the socket), or declines, through the web UI or the socket. While an approve is being checked, every other resolution of that approval is refused with `APPROVAL_BUSY`; retry it a moment later, once the approve ends approved or pending again.
4. `EvidenceService.AddEvidence` with the envelope the key signed and the approved revision.

The approved key is certified for 2 minutes with presence `user_verified`, and its session is never renewed. It signs exactly the approved statement, added with a revision of the approved change: any other statement or change is `PRESENCE_STATEMENT_MISMATCH`. Its first evidence object spends it, and adding that object again changes nothing. Through the web UI, `AddEvidence` accepts only evidence signed by such a key.

The WebAuthn challenge is derived, not stored, so any verifier recomputes it from the certificate:

```text
SHA-256(PAE("application/vnd.reef-vcs.presence-approval",
            "reef-presence-approval/v1\n<authority key id>\n<key id>\n<identity id>\nhuman\n<session>\n<statement SHA-256>\n<change id>\n"))
```

The touch therefore approves this key, this statement (its kind, result, target and tree) and this change, and nothing else.

### The certificate's evidence

A `user_verified` certificate's predicate carries the assertion beside `presence`:

```json
"presence": "user_verified",
"presenceEvidence": {
  "type": "webauthn",
  "passkey": "<the passkey's fingerprint>",
  "credentialId": "<base64url>",
  "publicKey": "<base64 of the DER SubjectPublicKeyInfo>",
  "algorithm": -7,
  "rpId": "localhost",
  "approves": {"statement": "<SHA-256 of the statement payload>", "change": "<the change id, 32 hex>"},
  "authenticatorData": "<base64>",
  "clientDataJSON": "<base64>",
  "signature": "<base64>"
}
```

`presenceEvidence` is required exactly when `presence` is `user_verified`, and refused otherwise; `user_verified` is certified for actor `human` only. After the authority's signature, a verifier derives the challenge from the certificate's own fields and checks the assertion:

- `clientDataJSON`'s `type` is `webauthn.get`, its `challenge` is the derived one, and its `origin`'s host is `rpId` (over `http` for `localhost` only), not from a cross-origin frame;
- `authenticatorData` starts with the SHA-256 of `rpId`, with the user-present and user-verified flags set;
- `signature` verifies over `authenticatorData` and the SHA-256 of `clientDataJSON`, with `publicKey` by `algorithm`;
- `passkey` is the SHA-256 of `publicKey`.

`reef_evidence::session::check_certificate` does that in Rust, and `certify` runs the same check before the authority signs. A reader older than R3.43 does not know `user_verified` and refuses the certificate. At approval the daemon also checks what the certificate cannot show: the passkey is a live one of the identity, then and when the key is stored, the origin is exactly `http://localhost:<the listener's port>`, and the sign count went up (or both counts are 0, as synced passkeys report). With an EdDSA passkey a certificate is about 1 KB larger.

## What a verifier sees

Every evidence read reports `identity` beside `signature`: the certified key (identity, kind, actor, agent, session, presence, issuer, window; for a `user_verified` key also its passkey, what it approved, the approval and the web UI session) and a status, judged at the time the daemon first witnessed the evidence (its `added_at`), never at a time the claim states:

| Status | Meaning |
| --- | --- |
| `unbound` | No certificate for the key: a bare key (`--key`, `--envelope`) |
| `bound` | The certificate verifies, the evidence was witnessed within the key's window and before every revocation that applies |
| `expired` | The certificate verifies, but the evidence was witnessed outside the window |
| `revoked` | The evidence was witnessed at or after a revocation's `invalid_from` |
| `invalid` | The certificate does not verify: a stored row was changed, or its authority is unknown |

A keyless key reports `issuer_kind: sigstore` and `sigstore` (the OIDC issuer and subject, the match, the log index, Fulcio's CI claims). A revocation made after the evidence was witnessed is still reported, beside `bound`. Evidence whose own signature does not verify reads `invalid` with no key: its key id is not believed. A key whose certificate does not verify reports only its id and the daemon's records of it, and never matches an identity filter. The same envelope added to a second repository after its key expired reads `expired` there: each repository's row has its own `added_at`.

A `user_verified` key's presence is printed with its passkey's label. `reef evidence show` also says whether the policy at `trunk()` pins it: `presence user_verified (passkey "MacBook Touch ID", pinned here)`, or `(passkey "MacBook Touch ID", not pinned by this repository's policy)`. With no policy at `trunk()`, and in `reef identity session show`, the line names the passkey only. `EvaluatePolicy` reports the same per signer, and `reef policy eval` prints it. A read that finds the key on evidence other than the statement and change it approved, which only a database changed behind the daemon can hold, reports that object's presence as `none`.

## Revocation

A revocation records when it was made and `invalid_from`, from when signatures stop counting. By default that is now: evidence the daemon saw earlier keeps its identity, and a reader sees that the key was revoked later. A compromise backdates it:

```sh
reef identity session revoke <key id> --since 2026-11-05T09:00:00Z   # the key leaked then
reef identity session revoke --session ses_41c9e07ab2d35f18 --since-issued
reef identity revoke alice --since-created
```

Revoking a session covers every key in it and ends its renewal; revoking an identity ends its issuance and covers all its keys; a remote token linked to it keeps working for reads and writes until it is revoked too (`reef identity revoke` prints the line). A later revocation can move `invalid_from` earlier, never later.

## What presence proves

On one machine, anything running as the daemon's user can do whatever the user can on the socket, including getting a key declared `human` for the user's identity, and can read the authority's key file. What an identity tells apart on its own: other local users, remote callers (whose linked token decides the identity), and CI, whose keys need its token. Presence says more about a person:

- **`presence: none`** means the daemon's user, or something running as that user, asked for the key. Its `human` is a declaration, and `reef evidence show` prints it as "not verified".
- **`presence: user_verified` with a pinned passkey** means that a passkey the repository's policy pins for the identity verified a person (a touch with a fingerprint, a face or a PIN) for this one statement about this one change, within the 2 minutes the approval waited. The certificate carries the assertion and the passkey's public key, so the pin, not the daemon's database or the authority key, decides whose passkey it was. An agent cannot produce it alone: it would need the person's authenticator.
- **`presence: user_verified` with a passkey the policy does not pin** counts as `none` for that policy. Only the authority says the passkey is the identity's, and anything that can read the authority's key file, or that registered the identity's first passkey before the person did, could have made that true.

A pinned `user_verified` sign-off does not prove that the person read what they approved. An agent can start an approval when the person expects one and hope for a touch. The approval page names the identity, the change, its tree, the kind, the result and who asked; read it before you touch, and decline an approval you did not start.
