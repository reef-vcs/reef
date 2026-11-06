# reef-evidence

The evidence format of **Reef**, version control built for a world where AI agents write most of the code ([ADR 0055](../../docs/adr/0055-evidence-object-format.md)). An evidence object is a claim about one git tree (a test, build, lint or policy check, and whether it passed, failed, or reached no verdict), written as an in-toto Statement v1 with Reef's predicate `urn:reef-vcs:evidence:v1` and signed once with ed25519 in a DSSE envelope.

The crate builds and signs claims, checks envelopes in DSSE's order (the envelope, the key, the signature, and only then the payload), and writes the canonical envelope whose SHA-256 is the evidence id. Keys are ed25519 through ring: private keys as PKCS#8 PEM, public keys as DER SubjectPublicKeyInfo, and key ids as the SHA-256 of that DER. A signature that verifies says who signed, not that the claim is true or the signer trusted.

It depends on no Reef crate but `reef-error`, and on no jj-lib, gRPC, SQLite or tokio, so an executor or a store can read the format without the Reef client. [`docs/evidence.md`](../../docs/evidence.md) describes the format for someone writing an executor or a script. The design document and roadmap are at <https://github.com/reef-vcs/reef>.
