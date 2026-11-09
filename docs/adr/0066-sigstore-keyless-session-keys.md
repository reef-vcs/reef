# 0066. Sigstore keyless session keys

- Status: accepted
- Date: 2026-11-08
- Ticket: R3.42
- Amends: [0057](0057-operator-identities-and-session-keys.md) (a second issuer kind, `sigstore`; identities gain OIDC matches; a binding's revocation beside the key's, session's, identity's and authority's), [0055](0055-evidence-object-format.md) (a fourth predicate type, the binding statement; a feature-gated `sigstore` module in the format crate), [0029](0029-daemon-api-v1.md) (decision 12's exceptions gain `RegisterSigstoreKey`), [0061](0061-repository-policy-file.md) (attestor sets gain `issuer_kinds`; the input's key gains `issuer_kind`, `oidc_issuer` and `oidc_subject`)

## Context

[0057](0057-operator-identities-and-session-keys.md) binds a session key to an operator identity through one issuer, the daemon's authority key, and leaves Sigstore keyless as a second issuer kind. Its main use is CI evidence: today a CI job's evidence is bound to a `ci` identity because the executor dials the remote listener with a token linked to it. That token is a long-lived secret in the CI system, and whoever holds it can mint keys for the identity a policy that "trusts its CI identity and nothing else" accepts.

Keyless moves the CI identity from "who holds Reef's token" to "what the CI provider's OIDC issuer says about this job". Sigstore's Fulcio certifies a key against an OIDC token, for 10 minutes, with the token's issuer and subject in the certificate; Rekor logs what the key signs, in public. A Rust verifier exists, `sigstore-verify` (sigstore-rust 0.15.1, Apache-2.0, MSRV 1.86): it verifies a bundle offline against a `trusted_root.json`, has no ring backend but takes a caller's `CryptoProvider`, and resolves 52 dependencies with no aws-lc, OpenSSL, tokio or reqwest when its features are off. Its signing crate signs only P-256, through digests ring cannot sign, over reqwest whose provider is aws-lc. A staging and public-good check on 2026-11-08 showed that Fulcio certifies an ed25519 key and Rekor v1 logs a DSSE envelope signed by one.

[0025](0025-agent-facing-cli-contract.md) holds the CLI to "never interactive", which a person's browser login would break.

## Decision

1. **Fulcio certifies the session key, once, and Rekor logs its binding statement; evidence is unchanged.** The signing process generates an ed25519 key, gets an OIDC token for audience `sigstore`, has Fulcio certify the key (proving it with a signature over the token's `email`, else its `sub`), signs a binding statement about the key (an in-toto Statement v1, predicate type `urn:reef-vcs:sigstore-session-key:v1`, subject the key's id, predicate `actor`, `agent`, `session`, `presence` `none`, `custody` `client`) as a DSSE envelope, logs it in Rekor v1 (`dsse` 0.0.1), and registers the Sigstore bundle v0.3 with the daemon (`RegisterSigstoreKey`). Evidence signed by the key is [0055](0055-evidence-object-format.md)'s, signed and added as before. One Fulcio certificate and one Rekor entry per key, not per object, and the bundle is a standard one any Sigstore verifier checks.
2. **Verification is offline, through `sigstore-verify` with a ring provider, against a vendored trusted root.** `reef-evidence`'s `sigstore` module (feature `sigstore`, which the daemon enables and the `reef` binary does not) implements `sigstore_crypto::CryptoProvider` over ring and pins `sigstore-verify`, `-trust-root` and `-crypto` `=0.15.1` with no default features. `check_sigstore_bundle` checks the size, the bundle, the chain to a Fulcio CA of the root at the signing time, the SCT, the log entry (inclusion proof, checkpoint, SET), the signing time within the leaf's window, the envelope's signature by the leaf's key (whose SPKI hash is the statement's subject), then that the key is ed25519 and the predicate is known. The public-good root is vendored in Reef's repository (`scripts/sigstore-root.sh` refreshes it, and a weekly workflow warns when it is stale), `[sigstore] trusted_root` overrides it, the daemon never fetches anything, and every read verifies the stored bundle again.
3. **An identity names the exact OIDC issuer and subject it accepts.** `identity_oidc` holds matches (`oidc_<16 hex>`, an `https://` issuer, a subject of visible ASCII), at most one live match per pair across all identities, added at `CreateIdentity` or by `AddOidcMatch` and revoked by `RevokeOidcMatch` (ADMIN, never remote, never an MCP tool). Matching is byte for byte, against the certificate's issuer extension (`.1.8`, else `.1.1`) and its SAN, with no patterns. A key's match is fixed at registration and recorded in its row; revoking the match is a revocation with [0057](0057-operator-identities-and-session-keys.md)'s `invalid_from` rules that applies to every key bound through it.
4. **Registration goes through any `write` channel, and the OIDC identity decides.** The socket and every `write` token register; a `ci` identity's keyless key is allowed over the socket, unlike a daemon-issued one, because the proof is Fulcio's and Rekor's. Every bound field is signed by the key, so registering someone else's public bundle gains nothing, and the same bundle again returns the record (`created` false). `human` is never bound to a `ci` identity, and 10,000 live keys per identity still apply.
5. **The session is named in the signed statement.** A new session id creates a session with channel `sigstore`; a known one must be a keyless session with the same identity and actor, unrevoked. So whoever registers a bundle cannot regroup keys into another session.
6. **The window is Fulcio's, judged at witness time.** A keyless key's `notBefore` and `notAfter` are the leaf's; evidence is judged at the daemon's `added_at`, as [0057](0057-operator-identities-and-session-keys.md) decides, and Rekor's time witnesses only that the certificate was used in its window. Keys are had when needed: the executor gets one when a job first signs and renews in its session when less than three minutes remain.
7. **The CLI runs no login.** A token comes from a file (`--oidc-token-file`, `0600`), `SIGSTORE_ID_TOKEN`, or GitHub Actions' token endpoint, through one table of sources; nothing opens a browser or waits for a person. A person's keyless key is verified like a CI's; the browser login belongs in the web UI or desktop shell.
8. **Keyless keys are ed25519,** as every Reef key is ([0055](0055-evidence-object-format.md) decision 4). A P-256 leaf is refused.

A Fulcio CA a keyless key came from is recorded in `identity_authorities` with `kind` `sigstore` and `retired_at` set, so it never issues; `ListAuthorities` shows it, policy can pin its key id, and `RevokeAuthority` revokes it locally. Policy attestor sets gain `issuer_kinds`, and the input's key gains `issuer_kind`, `oidc_issuer` and `oidc_subject`, so "trust `ci-main`, keyless only" is a policy.

## Consequences

- A stolen Reef token no longer suffices for CI evidence a keyless-only policy counts: that needs the job's OIDC token, which only the job can get and which the executor keeps from checks.
- The binding is public: the certificate (workflow, repository, or email) and the statement's hashes are in Rekor and the CT log, for private repositories too, unless `[sigstore] trusted_root` names a private Sigstore.
- A stale vendored root fails closed: a new Rekor shard's bundles do not verify until Reef updates or the root is overridden.
- The daemon parses X.509, JSON and Merkle proofs from `write` callers, bounded at 64 KiB and checked signature first.
- Ten-minute keys make the daemon's clock matter more: a skewed clock reads fresh keyless evidence `expired` sooner.
- Reef owns a ring `CryptoProvider` and pins a pre-1.0 dependency whose minor releases break.

## Alternatives considered

- **A Sigstore bundle per evidence object.** cosign-compatible per object, but a Rekor entry per object and a new evidence path; a follow-up for evidence other tools sign.
- **The `sigstore` crate (sigstore-rs).** OpenSSL or aws-lc only, about 240 dependencies including `rsa` (RUSTSEC-2023-0071), no Rekor v2 or RFC 3161 verification.
- **`sigstore-sign`.** P-256 only, signs through digests ring cannot, takes no caller key, and defaults to aws-lc, TUF and a browser opener.
- **aws-lc for signing only.** Breaks the ring-only rule for the `reef` binary.
- **TUF in the daemon.** Network access, tokio and reqwest in the verifier; a refresh command is a follow-up.
- **The root embedded in `sigstore-verify`.** It changes only with that crate's releases.
- **Pattern or regex matches.** A pattern that admits a branch or pull request's workflow admits that branch's code.
- **Matches in each repository's policy instead of on identities.** Policy already names identities, and raw OIDC strings in policy would bypass revocation and the identity's record.
- **Registration only through a token linked to the matched identity.** Adds no proof and keeps the long-lived secret keyless removes.
- **An interactive or device-code login in the CLI.** Breaks [0025](0025-agent-facing-cli-contract.md); the device flow still waits on a person, and Sigstore's issuer support for it is unverified.
- **P-256 session keys.** Fulcio and Rekor accept ed25519, and P-256 would touch [0055](0055-evidence-object-format.md)'s key rule and `AddEvidence`.
