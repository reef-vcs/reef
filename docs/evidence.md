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
- **`artifacts`** (optional): at most 64 things the check produced, each with a `name` and a `sha256` digest, and optionally `uri`, `mediaType` and `annotations`. Reef records the digests; it does not need the bytes, and stores them when they are uploaded ([Artifacts](#artifacts)).
- **`annotations`** (optional): an object for your own data. Reef never reads it and never defines a field in it.

There is no field for who attested: anyone can write any name. The attestor is the key that signed.

The envelope is `{"payload": <base64 of the payload>, "payloadType": "application/vnd.in-toto+json", "signatures": [{"keyid": <key id>, "sig": <base64 signature>}]}`. The signature is ed25519 over DSSE's pre-authentication encoding of the payload, `"DSSEv1" SP LEN(type) SP type SP LEN(payload) SP payload`, each LEN the decimal byte length. The `keyid` is the SHA-256, in lowercase hex, of the signer's public key as DER SubjectPublicKeyInfo (`openssl pkey -pubin -outform DER | shasum -a 256`). Reef sends that public key beside the envelope and verifies with it; the `keyid` must match it but decides nothing.

Reef refuses an envelope over 64 KiB, JSON nested deeper than 32 levels, a key repeated in one object anywhere in the envelope or the payload, a second signature, and a small-order or non-canonical public key. It checks in DSSE's order: the envelope, the key, the signature, and only then the payload. `reef schema evidence` prints the Statement, the predicate and the envelope as JSON Schema; the schema describes the format, and the daemon's checks are the authority.

## Ids

Reef stores every envelope in one canonical form: compact JSON with exactly `payload`, `payloadType` and `signatures` (`keyid`, `sig`), in that order, with padded standard base64 and no trailing newline. Fields DSSE says to ignore are dropped. The payload is kept byte for byte as signed. The evidence id is the SHA-256 of the canonical envelope, so `reef evidence show <id> --envelope | shasum -a 256` prints the id, and an envelope added twice is stored once. ed25519 is deterministic, so the same key signing the same payload makes the same object.

## Keys

By default `reef evidence add` signs with a session key (R3.03): a fresh ed25519 key, generated in the command, certified by the daemon for an operator identity (yours by default, `--identity`, or through a remote daemon its token's) as `--actor` (`agent` by default, `executor`, or `human`), valid five minutes, used for the one envelope and never written anywhere. Evidence signed so reads `bound` to that identity. [docs/identity.md](identity.md) has identities, session keys, what each status means, and how a script in another language gets a key.

`reef evidence key generate <path>` writes a bare ed25519 key instead, for scripts and trials: a private key to `<path>` (PKCS#8 PEM, mode 0600, never overwriting a file) and its public key to `<path>.pub` (SPKI PEM), and prints the key id. Evidence signed with `--key` reads `unbound`: no identity answers for it. `reef evidence add --key` also reads OpenSSL 3's `openssl genpkey -algorithm ed25519` keys, and refuses a key file its group or others can read. macOS's `/usr/bin/openssl` is LibreSSL, which cannot make ed25519 keys; Homebrew's `openssl@3` can.

## Adding evidence

From a workspace, `reef evidence add` reads the revision (`@` by default, snapshotting first), builds the claim about its tree, signs it, and sends it with the commit id it read:

```sh
cargo test 2>&1 | tee test.log
reef evidence add --kind test --result pass --actor executor --artifact test.log
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
reef evidence key generate ~/.config/reef/ci.pem
tree=$(reef show -o json | jq -r .tree_id)
sh sign.sh ~/.config/reef/ci.pem "$tree" lint pass > envelope.json
reef evidence add --envelope envelope.json --public-key ~/.config/reef/ci.pem.pub -r @
```

With `-r`, the evidence is linked to that change as well as to its tree, and Reef refuses it when the revision's tree is not the subject. Without `-r`, it is added by tree alone, as an executor adds it. The API is `EvidenceService.AddEvidence`: the envelope's bytes, the public key's DER, and an optional revision.

## Reading evidence

`reef evidence list` lists the repository's evidence, newest first; `-r <rev>` the evidence on a revision's tree, `--tree <id>` on a tree, and `--change <id>` the evidence added for a change on any tree it had, each marked `(not current)` once the change's tree has moved on. `reef evidence show <id>` prints one object, its statement exactly as signed, `signature: valid` or `invalid`, checked as it is read, and `identity:`, whom the key is bound to and its status when the daemon first saw the object ([docs/identity.md](identity.md#what-a-verifier-sees)). `--identity <name>` lists the evidence of keys certified for that identity. Every field Reef reports is read from the signed payload, so a stored row changed behind the daemon reads `invalid` with what was signed; adding the genuine envelope again repairs it. `--key` filters take the full 64-digit key id. It exits 0 either way: `invalid` means the stored object was changed, and is data about it.

Evidence binds to the tree, so it stays on a change across a describe, or a rebase that keeps the tree, and two changes with the same tree share it. Abandon and undo leave it alone; garbage collection deletes it only once it is off live work and past its retention ([Retention](#retention)); backups carry it; `reef repo forget` deletes it.

## Artifacts

An artifact is a file a check produced, such as a log, a report or a build output, named in the predicate's `artifacts` by its SHA-256. The daemon stores the bytes once per repository, under `<state>/repos/<id>/artifacts/`, and keeps them by retention. [ADR 0056](adr/0056-evidence-artifact-store-and-retention.md) records why.

`reef evidence add --artifact <file>` hashes each file for the claim, uploads it, and only then adds the evidence, so evidence Reef's own producer adds never names bytes the store lacks. A file that changes between the hash and the upload is refused (`ARTIFACT_DIGEST_MISMATCH`) and nothing is added. `--no-upload` records the digest only. With `--envelope`, each `--artifact` file must be one the envelope names, or nothing is sent. `reef evidence artifact upload <file>...` uploads files without evidence, for a script that signs elsewhere, and prints one line per file: its digest's first 12 digits, its size, and `stored` or `already stored`. An upload resumes where an interrupted one stopped, and bytes already stored are not sent again.

```sh
reef evidence add --kind test --result pass --artifact test.log
reef evidence show <id>                        # artifact test.log sha256:… stored 41 KiB
reef evidence artifact read <sha256> > test.log
reef evidence artifact show <sha256>           # its state, size, times, and the evidence naming it
```

`reef evidence artifact read <sha256>` writes the bytes to stdout, from `--offset` for `--length` bytes (to the end by default); with `-o json` or `-o brief` it prints the artifact's record instead, as `show` does, without reading the bytes. A digest may be a unique prefix of at least 4 hex digits of a stored or collected artifact. When a whole read finds that the stored bytes no longer hash to the digest, the command fails after writing them and says on stderr that they are not the artifact: discard them.

`reef evidence show` prints each artifact as `stored <size>`, `collected <date>` (retention removed the bytes, or Reef dropped them as damaged or missing) or `absent` (never uploaded, or deleted when no evidence named it), read from the store's index, not from the signed claim.

### The upload protocol

An executor in another language uploads through `ArtifactService.UploadArtifact`, one unary call per chunk (gRPC-web carries no client streaming). Every call names the repository, the whole artifact's `sha256` (64 lowercase hex digits) and `size`, an `offset`, and `data`:

1. Hash the file.
2. Probe: a call at `offset` 0 with no `data`. If the digest is stored, the answer has `received` equal to the size, the `artifact` record and `created: false`, and the upload is done: stored bytes are never sent again. Otherwise `received` is the bytes a partial upload of this digest and size holds (0 for none).
3. Send the bytes from `received` in chunks of at most 1 MiB, each at the `offset` the previous answer's `received` gives. A chunk at another offset is refused with `ARTIFACT_OFFSET_MISMATCH`, whose `received` metadata says where to resume; this is how a client recovers from a lost response, or from another client uploading the same bytes, which shares the partial upload.
4. The call whose bytes complete the size answers with the `artifact` record and `created: true`. If the bytes hash to another digest, it is refused with `ARTIFACT_DIGEST_MISMATCH` (`actual_sha256`) and the partial upload is deleted: start again from byte 0.

A zero-byte artifact is one call with no data. A partial upload survives a daemon restart, so a client resumes from the probe's `received`, and is deleted a day after its last chunk. A size over `[artifacts] max_object_size` is refused with `ARTIFACT_TOO_LARGE` before anything is written; a chunk over 1 MiB, or one reaching past the size, with `INVALID_ARTIFACT_UPLOAD`. Uploads record no operation. `reef_client::Client::upload_artifact_file` is this loop in Rust.

`ArtifactService.ReadArtifact` streams the bytes in messages of at most 1 MiB; the first carries the artifact's record. A read of the whole artifact is hashed as it is sent: if the bytes no longer match, the stream ends with `ARTIFACT_DAMAGED` (`damage` `digest`, `actual_sha256`) after the data, and the daemon drops the damaged bytes: the artifact reads as collected, and the next upload of the genuine bytes stores them again. A stored artifact whose file is gone is `ARTIFACT_DAMAGED` (`damage` `missing`), and one whose file's length is not its size `ARTIFACT_DAMAGED` (`damage` `size`, `actual_size`), before any data; an upload of the genuine bytes repairs either. A collected artifact is `ARTIFACT_COLLECTED`, and one evidence only names is `ARTIFACT_NOT_FOUND`. `GetArtifact` returns the record alone, with how many evidence objects name the artifact and the newest 20 of their ids.

## Retention

Garbage collection (`reef ws gc`, and the daemon every `[gc] interval`) deletes evidence and artifacts that no live work keeps. Live work is the revset `trunk() | bookmarks() | working_copies() | mutable()`, evaluated with Reef's aliases: `reef log`'s default set plus every local bookmark. Evidence is pinned, and never deleted, when its tree is a live commit's tree, or when it was added for a change (`-r`) that is still live, on any tree that change had.

Evidence that is not pinned is deleted once it was first added longer ago than `[evidence] retention`. Re-adding it does not renew it. Then artifacts, by what names them:

| The artifact is | It goes when | What remains |
| --- | --- | --- |
| Named by no evidence | a day after it was last used, at the soonest | nothing |
| Named only by evidence that is not pinned | last used longer ago than `[artifacts] retention` | a record that reads `collected` |
| Named by pinned evidence | never, by age | the bytes |

An artifact is used when it is stored, uploaded again, or named by evidence added. While a repository's stored artifacts pass `[artifacts] max_size`, the least recently used of those not named by pinned evidence are collected too, except what was used within the last day. That day is a grace of its own, so an upload waiting for the evidence that names it is kept even with `[gc] undo_window = "0s"`. Every cutoff is floored at GC's `keep_newer`: nothing added or used within `[gc] undo_window`, or within `[backup] retention` while the last backup is that recent, is deleted, whatever the keys say. `reef ws gc --dry-run` reports what would go.

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `[evidence] retention` | `"90d"` | `0s`, or 1 day to 10 years | Evidence not on live work is deleted this long after it was added. `0s` keeps all evidence. |
| `[artifacts] retention` | `"30d"` | `0s`, or 1 day to 10 years | Artifacts named only by evidence not on live work are collected this long after they were last used. `0s`: never by age. |
| `[artifacts] max_size` | `"10GiB"` | at least 64 MiB | The store's size per repository that GC collects down to; a target, not a quota. |
| `[artifacts] max_object_size` | `"1GiB"` | 1 KiB to 64 GiB | The largest artifact an upload accepts. |

What a reader sees afterwards: deleted evidence is `EVIDENCE_NOT_FOUND`, with no record kept; an artifact whose bytes were collected reads `collected` with its date, and reading it is `ARTIFACT_COLLECTED`, until the same bytes are uploaded again. Collected also covers bytes Reef found damaged or missing and dropped: a whole read that found them failing their digest, or garbage collection finding a stored artifact's file gone. Evidence on an old trunk commit that no bookmark or live change keeps expires after `[evidence] retention`; the landing service (R3.13) will pin the evidence a landing used.

## Stability

Format 1 is fixed: Statement v1, the payload type, one signature, the subject rule, the v1 predicate type and its fields, SHA-256 ids over the canonical envelope, and SPKI key ids. A later Reef only adds optional predicate fields, which never change what an existing field means, and a reader ignores fields it does not know, so evidence you write today stays valid. A change of meaning would be a new predicate type, read beside v1. Ids never change.
