# Identities and session keys

An evidence object says which key signed it. An **operator identity** says whom that key answers to: a person, or a CI pipeline. The daemon certifies short-lived **session keys** for identities, and every evidence read reports the identity its key is bound to. [ADR 0057](adr/0057-operator-identities-and-session-keys.md) records why; [docs/evidence.md](evidence.md) has the evidence format.

A certificate says which identity a key is bound to and what the requester declared it for. It never says that the identity should be trusted: a repository's policy decides that (R3.07).

## Identities

An identity is `person` or `ci`, with a name (`[a-z0-9][a-z0-9._-]{0,62}`, not starting with `idn_` or `ses_`, which mark ids) that is never reused in the daemon's database, even after the identity is revoked, so a policy that names `alice` cannot be satisfied by a newcomer. Restoring an older backup can bring a name back with a new id, so a policy that must not be fooled pins the identity's id and the authority's key id, not the name alone. Agents are not identities: an agent acts for an identity, and a key records it as the declared `actor` (`agent`, `executor`, or `human` for the person signing directly) and its label as `agent`.

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
| A remote token linked to identity X | X only | X only | never |
| A remote token linked to none | none | none | never |

Link a token when you create it: `reef remote token create ci --scope write --identity ci-main`. That is a `ci` identity's only way to get keys, so a local agent cannot mint "CI" evidence with one call.

### Getting a key

- `reef evidence add` issues one for five minutes, signs the one envelope, and drops it. `--identity` and `--actor` choose what it is certified for; `--key` signs with a key file instead.
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

`reef schema evidence session_key_predicate` prints its JSON Schema. A verifier checks it as evidence is checked (the envelope, the authority found by `keyid`, the signature, then the statement) and refuses a `presence`, `custody`, `actor`, kind or issuer it does not know; `reef_evidence::session::check_certificate` does that in Rust.

## What a verifier sees

Every evidence read reports `identity` beside `signature`: the certified key (identity, kind, actor, agent, session, presence, issuer, window) and a status, judged at the time the daemon first witnessed the evidence (its `added_at`), never at a time the claim states:

| Status | Meaning |
| --- | --- |
| `unbound` | No certificate for the key: a bare key (`--key`, `--envelope`) |
| `bound` | The certificate verifies, the evidence was witnessed within the key's window and before every revocation that applies |
| `expired` | The certificate verifies, but the evidence was witnessed outside the window |
| `revoked` | The evidence was witnessed at or after a revocation's `invalid_from` |
| `invalid` | The certificate does not verify: a stored row was changed, or its authority is unknown |

A revocation made after the evidence was witnessed is still reported, beside `bound`. Evidence whose own signature does not verify reads `invalid` with no key: its key id is not believed. A key whose certificate does not verify reports only its id and the daemon's records of it, and never matches an identity filter. The same envelope added to a second repository after its key expired reads `expired` there: each repository's row has its own `added_at`.

## Revocation

A revocation records when it was made and `invalid_from`, from when signatures stop counting. By default that is now: evidence the daemon saw earlier keeps its identity, and a reader sees that the key was revoked later. A compromise backdates it:

```sh
reef identity session revoke <key id> --since 2026-11-05T09:00:00Z   # the key leaked then
reef identity session revoke --session ses_41c9e07ab2d35f18 --since-issued
reef identity revoke alice --since-created
```

Revoking a session covers every key in it and ends its renewal; revoking an identity ends its issuance and covers all its keys; a remote token linked to it keeps working for reads and writes until it is revoked too (`reef identity revoke` prints the line). A later revocation can move `invalid_from` earlier, never later.

## What `presence: none` does not prove

On one machine, anything running as the daemon's user can do whatever the user can on the socket, including getting a key declared `human` for the user's identity, and can read the authority's key file. A `human` key today means the daemon's user, or something running as that user, asked for it, and every such key carries `presence: none`, which `reef evidence show` prints as "not verified". What an identity does tell apart now: other local users, remote callers (whose linked token decides the identity), and CI, whose keys need its token. Verified human presence, a passkey's user-verified assertion kept in the certificate, is R3.43.
