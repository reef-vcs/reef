# Evidence

An evidence object is a signed claim that a check ran on one tree and how it came out: a test suite passed, a build failed, a linter could not finish. Reef stores evidence beside the changes it is about, and later tickets gate landing on it. This page is for someone writing an executor or a script that produces evidence. [ADR 0055](adr/0055-evidence-object-format.md) records why the format is what it is.

A signature that verifies says that the holder of a key signed the claim. It does not say who holds the key, that anyone should trust it, or that the claim is true. Until repository policy decides which keys count (R3.07), any key's evidence is stored and none of it decides anything.

## The format

An evidence object is a [DSSE](https://github.com/secure-systems-lab/dsse) v1 envelope with exactly one ed25519 signature. Its payload is an [in-toto Statement v1](https://github.com/in-toto/attestation/blob/main/spec/v1/statement.md) about one git tree, with Reef's predicate type `urn:reef-vcs:evidence:v1`:

```json
{
  "_type": "https://in-toto.io/Statement/v1",
  "subject": [{"name": "tree", "digest": {"gitTree": "4b825dc642cb6eb9a060e54bf8d69288fbee4904"}}],
  "predicateType": "urn:reef-vcs:evidence:v1",
  "predicate": {
    "kind": "test",
    "result": "pass",
    "target": "crates/reef-lib:jj_writes",
    "startedOn": "2026-11-05T10:00:00Z",
    "finishedOn": "2026-11-05T10:01:12Z",
    "artifacts": [{"name": "test.log", "digest": {"sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"}, "mediaType": "text/plain"}],
    "annotations": {"runner": "ci-7"}
  }
}
```

- **Subject**: exactly one, named exactly `tree`, whose only digest is `gitTree`: the tree id in lowercase hex, as `git rev-parse <commit>^{tree}` prints it (40 digits in a SHA-1 repository, 64 in a SHA-256 one). The tree must be in the repository the evidence is added to.
- **`kind`** (required): what was checked. `test`, `build`, `lint` and `policy` mean what they say; any other kind matching `^[a-z][a-z0-9-]{0,62}$` is accepted.
- **`result`** (required): `pass`, `fail`, or `error`. `error` means the check reached no verdict (the runner crashed, timed out, or ran out of budget); it is not a failure of the tree.
- **`target`** (optional): the test or build target covered, in your own naming, 1 to 512 bytes without control characters. Leave it out for the whole check of that kind.
- **`startedOn`, `finishedOn`** (required): `YYYY-MM-DDTHH:MM:SS` with an optional fraction of one to nine digits and `Z`, such as `2026-11-05T10:00:00Z` or `2026-11-05T10:00:00.250Z`; no other offset, no leap second (`:60`), and a year from 1970 to 9999. The finish is not before the start.
- **`artifacts`** (optional): at most 64 things the check produced, each with a `name` and a `sha256` digest, and optionally `uri`, `mediaType` and `annotations`. Reef records the digests; it does not need the bytes.
- **`annotations`** (optional): an object for your own data. Reef never reads it and never defines a field in it.

There is no field for who attested: anyone can write any name. The attestor is the key that signed.

The envelope is `{"payload": <base64 of the payload>, "payloadType": "application/vnd.in-toto+json", "signatures": [{"keyid": <key id>, "sig": <base64 signature>}]}`. The signature is ed25519 over DSSE's pre-authentication encoding of the payload, `"DSSEv1" SP LEN(type) SP type SP LEN(payload) SP payload`, each LEN the decimal byte length. The `keyid` is the SHA-256, in lowercase hex, of the signer's public key as DER SubjectPublicKeyInfo (`openssl pkey -pubin -outform DER | shasum -a 256`). Reef sends that public key beside the envelope and verifies with it; the `keyid` must match it but decides nothing.

Reef refuses an envelope over 64 KiB, JSON nested deeper than 32 levels, a key repeated in one object anywhere in the envelope or the payload, a second signature, and a small-order or non-canonical public key. It checks in DSSE's order: the envelope, the key, the signature, and only then the payload. `reef schema evidence` prints the Statement, the predicate and the envelope as JSON Schema; the schema describes the format, and the daemon's checks are the authority.

## Ids

Reef stores every envelope in one canonical form: compact JSON with exactly `payload`, `payloadType` and `signatures` (`keyid`, `sig`), in that order, with padded standard base64 and no trailing newline. Fields DSSE says to ignore are dropped. The payload is kept byte for byte as signed. The evidence id is the SHA-256 of the canonical envelope, so `reef evidence show <id> --envelope | shasum -a 256` prints the id, and an envelope added twice is stored once. ed25519 is deterministic, so the same key signing the same payload makes the same object.

## Keys

`reef evidence key generate <path>` writes an ed25519 private key to `<path>` (PKCS#8 PEM, mode 0600, never overwriting a file) and its public key to `<path>.pub` (SPKI PEM), and prints the key id. It is a local key for scripts and trials until session keys bound to identities arrive (R3.03). `reef evidence add --key` also reads OpenSSL 3's `openssl genpkey -algorithm ed25519` keys, and refuses a key file its group or others can read. macOS's `/usr/bin/openssl` is LibreSSL, which cannot make ed25519 keys; Homebrew's `openssl@3` can.

## Adding evidence

From a workspace, `reef evidence add` reads the revision (`@` by default, snapshotting first), builds the claim about its tree, signs it, and sends it with the commit id it read:

```sh
reef evidence key generate ~/.config/reef/ci.pem
cargo test 2>&1 | tee test.log
reef evidence add --kind test --result pass --key ~/.config/reef/ci.pem --artifact test.log
reef evidence list -r @
```

An envelope signed elsewhere is added with `--envelope` and the signer's public key. This script signs a claim with OpenSSL 3:

```sh
#!/bin/sh
# sign.sh KEY TREE KIND RESULT: a Reef evidence envelope on stdout.
# Needs OpenSSL 3 (macOS's /usr/bin/openssl is LibreSSL, which has no ed25519).
set -eu
key=$1 tree=$2 kind=$3 result=$4
type=application/vnd.in-toto+json
now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
printf '{"_type":"https://in-toto.io/Statement/v1","subject":[{"name":"tree","digest":{"gitTree":"%s"}}],"predicateType":"urn:reef-vcs:evidence:v1","predicate":{"kind":"%s","result":"%s","startedOn":"%s","finishedOn":"%s"}}' \
  "$tree" "$kind" "$result" "$now" "$now" > "$dir/payload"
# DSSE's pre-authentication encoding: "DSSEv1 <len> <type> <len> <payload>".
{ printf 'DSSEv1 %s %s %s ' "${#type}" "$type" "$(wc -c < "$dir/payload" | tr -d ' ')"; cat "$dir/payload"; } > "$dir/pae"
openssl pkeyutl -sign -rawin -inkey "$key" -in "$dir/pae" -out "$dir/sig"
keyid=$(openssl pkey -in "$key" -pubout -outform DER | openssl dgst -sha256 -r | cut -d' ' -f1)
printf '{"payload":"%s","payloadType":"%s","signatures":[{"keyid":"%s","sig":"%s"}]}\n' \
  "$(openssl base64 -A -in "$dir/payload")" "$type" "$keyid" "$(openssl base64 -A -in "$dir/sig")"
```

```sh
tree=$(reef show -o json | jq -r .tree_id)
sh sign.sh ~/.config/reef/ci.pem "$tree" lint pass > envelope.json
reef evidence add --envelope envelope.json --public-key ~/.config/reef/ci.pem.pub -r @
```

With `-r`, the evidence is linked to that change as well as to its tree, and Reef refuses it when the revision's tree is not the subject. Without `-r`, it is added by tree alone, as an executor adds it. The API is `EvidenceService.AddEvidence`: the envelope's bytes, the public key's DER, and an optional revision.

## Reading evidence

`reef evidence list` lists the repository's evidence, newest first; `-r <rev>` the evidence on a revision's tree, `--tree <id>` on a tree, and `--change <id>` the evidence added for a change on any tree it had, each marked `(not current)` once the change's tree has moved on. `reef evidence show <id>` prints one object, its statement exactly as signed, and `signature: valid` or `invalid`, checked as it is read. Every field Reef reports is read from the signed payload, so a stored row changed behind the daemon reads `invalid` with what was signed; adding the genuine envelope again repairs it. `--key` filters take the full 64-digit key id. It exits 0 either way: `invalid` means the stored object was changed, and is data about it.

Evidence binds to the tree, so it stays on a change across a describe, or a rebase that keeps the tree, and two changes with the same tree share it. Abandon, undo and garbage collection leave it alone; backups carry it; `reef repo forget` deletes it.

## Stability

Format 1 is fixed: Statement v1, the payload type, one signature, the subject rule, the v1 predicate type and its fields, SHA-256 ids over the canonical envelope, and SPKI key ids. A later Reef only adds optional predicate fields, which never change what an existing field means, and a reader ignores fields it does not know, so evidence you write today stays valid. A change of meaning would be a new predicate type, read beside v1. Ids never change.
