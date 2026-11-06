# Issue #216 SDK remote tests make keys rustls can load

Phase 2 · Size S (no row; see "Size") · Fixes: issue #216 · Depends on: R2.08 (the remote listener and `[remote] tls_cert`/`tls_key`, ADR 0049), R2.06 and R2.07 (the SDKs' remote integration tests) · Unblocks: none listed

## Context

`remote repins after renewal` (`sdk/typescript/tests/integration/remote.test.ts`) failed on main at a749b0c, in `typescript 22.15.0 (macos-15)` on the self-hosted Mac ([run 42124067908](https://github.com/reef-vcs/reef/actions/runs/42124067908)). The daemon's remote listener never started: "the certificate and key do not make a usable pair: unexpected error: failed to parse private key as RSA, ECDSA, or EdDSA".

The SDKs' remote tests make P-256 keys with `openssl` and give them to the daemon as `tls_key`. On the self-hosted runner, `openssl` is macOS's `/usr/bin/openssl`, LibreSSL 3.3.6. LibreSSL writes the private scalar's minimal bytes. When the top byte is zero (1 key in 256), the ECPrivateKey's `privateKey` OCTET STRING is 31 bytes, not 32. RFC 5915 requires the fixed length, and ring, rustls's provider in the daemon, refuses the key. The test is at fault, not the daemon. The tests need keys that are always 32 bytes, whatever `openssl` the runner has.

This ticket gives each SDK's remote tests one key helper: Node's `crypto` in TypeScript, and in Python `openssl genpkey` that makes the key again if its scalar is short. Every key in those two files comes from the helper, and the helper checks every key it writes. Certificates still come from `openssl`. Tests only: no daemon, SDK, CI or docs change.

### What was verified while writing this ticket

All of this was checked at a749b0c (main), on macOS 27 with `/usr/bin/openssl` (LibreSSL 3.3.6), Homebrew's `openssl` (OpenSSL 3.6.3) and `openssl@1.1` (1.1.1w), Node 22.16.0, and `target/debug/reef-daemon` built from main. Scratch files were under the session's scratchpad and are not part of the change.

- **Where an openssl-made key reaches the daemon.** Only in two files. Each loads four such keys per run:

  | File | Test | Key | How it is made |
  | --- | --- | --- | --- |
  | `sdk/typescript/tests/integration/remote.test.ts` | `remote misdirected` (`:359`) | `key.pem` | `selfSigned()` (`:136-158`), `genpkey` (`:144`) |
  | | `remote ca verified` (`:381`) | `leaf.key` | `req -new -newkey ec` (`:404-418`) |
  | | `remote repins after renewal` (`:468`) | `key.pem`, then a new `key.pem` after `unlinkSync` (`:513-514`) | `selfSigned()` |
  | `sdk/python/tests/integration/test_remote.py` | `test_remote_ca_verified` (`:208`) | `leaf.key` | `req -new -newkey ec` (`:217-221`) |
  | | `test_remote_repins_after_renewal` (`:250`) | `key.pem`, then a new one after `unlink` (`:288-289`) | `self_signed()` (`:192-204`), `genpkey` (`:196-199`) |
  | | `test_remote_misdirected` (`:304`) | `key.pem` | `self_signed()` |

  The other remote tests use the daemon's generated certificate. So a leg on LibreSSL fails 1 run in 64 (1 − (255/256)⁴). A push to main runs four such legs on the self-hosted Mac (Node 22.15 and 26, Python 3.11 and 3.15), so about 6% of pushes have one failure.
- **Nowhere else.** A search for `genpkey`, `-newkey`, `ecparam`, `generateKeyPair`, `tls_key` and `PRIVATE KEY` outside `docs/` found only:
  - `crates/reef-daemon/tests/remote.rs`'s `pair()` (`:344-359`), which makes its key with `rcgen` (ring). That always writes 32 bytes.
  - `scripts/test-remote.sh`, which uses the generated certificate and calls `openssl` only for `base64` (`:138`, `:147`).
  - The SDKs' unit tests (below).

  `web/e2e`, the benches and the other scripts make no keys.
- **The unit tests never hand a key to rustls.** `tests/unit/tls.test.ts` (`:32-50`, server at `:72`) and `pinning.test.ts` (`:33-50`, server at `:61`) give their `genpkey` keys to `node:tls.createServer`, which uses Node's own OpenSSL. `tests/unit/test_tls.py` writes `key.pem` and only parses the certificate. A 31-byte LibreSSL key loads in Node (`createPrivateKey`, and a full `node:tls` handshake) and in Python's `ssl` (`load_cert_chain`, OpenSSL 1.1.1w). So these tests cannot hit #216, and they stay out of scope.
- **The rate, and that only LibreSSL has it.** In scratch loops, with each key's PKCS#8 checked for `02 01 01 04 20`:
  - `/usr/bin/openssl genpkey`: 6 of 1,500 keys short.
  - `/usr/bin/openssl req -newkey ec`: 4 of 1,000 short.
  - Homebrew OpenSSL 3.6.3 and `openssl@1.1`: 0 of 600 each.
  - Node `generateKeyPairSync("ec", { namedCurve: "P-256" })`: 0 of 20,000.

  A short key reads `30 81 86 … 04 6c 30 6a 02 01 01 04 1f`, against `30 81 87 … 04 6d 30 6b 02 01 01 04 20`. Both `genpkey` and `req -keyout` write PKCS#8 (`BEGIN PRIVATE KEY`).
- **The daemon refuses exactly that key, and accepts the same scalar padded.** A scratch daemon (`[remote] tls_cert`/`tls_key`, `listen = "127.0.0.1:0"`) given a 31-byte LibreSSL key logged the CI error word for word, retrying every 1 s, then 2 s. Given the same key re-encoded by OpenSSL 3 (`openssl pkey -in short.pem -out padded.pem`, which writes `04 20 00 …`), it logged "remote listener serving". A Node key served too. LibreSSL's own re-encoding (`/usr/bin/openssl ec`) keeps the 31 bytes.
- **LibreSSL signs with Node's keys.** With Node-written PKCS#8 keys and `/usr/bin/openssl`, all of these work:
  - `req -new -x509 -key` (self-signed).
  - `req -new -x509 -config ca.cnf -key ca.key` (the CA).
  - `req -new -key leaf.key` (the CSR).
  - `x509 -req -CA ca.pem -CAkey ca.key -extfile ext.cnf` (the leaf).

  The leaf verifies under LibreSSL and under OpenSSL 3 `verify -x509_strict`. Each certificate's public key equals its key's. The certificates' SPKI names the curve (`ASN1 OID: prime256v1`), because Node's key does, so the `-pkeyopt ec_param_enc:named_curve` workaround (`remote.test.ts:31-33`) is not needed for these keys.
- **How the daemon loads `tls_key`.** `remote/listener.rs:203-214` (`resolver`) calls `Resolver::supplied`, and `remote/tls.rs:268-292` (`load_pair`) reads the PEM with `PrivateKeyDer::from_pem_slice`, which accepts the short key. Then `CertifiedKey::from_der(chain, private, &provider())` with ring refuses it (`TlsError::Pair`). The listener retries with backoff (`listener.rs:236-239`), and the harness gives up after 30 s.
- **Python has no EC library to hand.** `sdk/python/pyproject.toml`'s dev group has no `cryptography`. `uv.lock` has `cryptography` 50.0.2 only through `twine` → `keyring` → `secretstorage`, marked `sys_platform == 'linux'`, so it is absent on the Mac. The SDK's DER reader `reef._tls._read` (`src/reef/_tls.py:26-43`) is stdlib-only, and the unit tests already import `reef._tls` (`tests/unit/test_tls.py:13`).
- **The CI legs and their `openssl`.** `ci.yml`'s `python` (`:413-462`) and `typescript` (`:464-521`) jobs run `just python-check` then `just python-test`, and `just typescript-check` then `just typescript-test`, on `ubuntu-24.04` and `macos-15`. `plan` (`:44-74`) sends the macOS legs to the self-hosted `reef-mac` for same-repository PRs and pushes, and to GitHub's `macos-15` for forks, Dependabot and the `macos` label.

  | Runner | `openssl` on PATH | Short keys |
  | --- | --- | --- |
  | ubuntu-24.04 | Ubuntu's OpenSSL 3 | no |
  | macos-15 (hosted) | Homebrew's, first on PATH (the tests' comments name `openssl@1.1`) | no |
  | reef-mac (self-hosted, #211) | `/usr/bin/openssl`, LibreSSL 3.3.6: the LaunchDaemon's PATH is `~/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin` (`scripts/mac-runner.sh:151`, `:172`) | 1 in 256 |

  So the flake started with #211. The recipes are in `justfile:114-126` (Python) and `:142-152` (TypeScript). Both integration recipes pass their arguments to the runner, so a single file can be run.

## Decisions

### Size

S, with no `dev-phases.md` row. Two test helpers and a scalar check in each SDK's remote integration file, their callers, and three small tests.

### TypeScript: Node makes the keys

In `remote.test.ts`, beside `selfSigned`:

```ts
/** A P-256 key, PKCS#8 PEM, made by Node. LibreSSL writes a 31-byte scalar 1 time in 256, which
 * rustls refuses, so no key the daemon loads comes from openssl (#216). */
function p256Key(path: string): void {
  const { privateKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const der = privateKey.export({ type: "pkcs8", format: "der" });
  if (scalarLength(der) !== 32) {
    throw new Error(`${path}: a ${scalarLength(der)}-byte P-256 scalar (#216)`);
  }
  writeFileSync(path, privateKey.export({ type: "pkcs8", format: "pem" }), { mode: 0o600 });
}
```

- **`scalarLength(pkcs8: Buffer): number`** is a small DER walk (about 15 lines): PrivateKeyInfo → skip `version` and `privateKeyAlgorithm` → the `privateKey` OCTET STRING → ECPrivateKey → skip `version` → the `privateKey` OCTET STRING's length. It handles one- and two-byte lengths. The TypeScript SDK has no DER reader to reuse (`src/tls.ts` uses `X509Certificate`). Node always writes 32 bytes, so the check never fires today. It is there so that a change of key source fails at the key, with the issue's number, and not as a listener that never starts 30 s later.
- **`selfSigned`** calls `p256Key(join(dir, key))` in place of `genpkey` (`:143-145`). Its `existsSync` reuse for renewals stays.
- **`remote ca verified`** makes `ca.key` and `leaf.key` with `p256Key`. It then runs `req -new -x509 -config ca.cnf -key ca.key …` and `req -new -key leaf.key -out leaf.csr -subj /CN=reefvm.test`, in place of the two `-newkey ec … -nodes -keyout` runs. The `x509 -req` step is unchanged.
- **`P256`** (`:31-33`) and its comment go, because nothing in the file uses them any more. `generateKeyPairSync` is imported from `node:crypto`.

### Python: make the key again if it is short

In `test_remote.py`, beside `self_signed`:

```python
def scalar_length(pem: bytes) -> int:
    """The private scalar's length in a PKCS#8 EC key: 32 for P-256 (RFC 5915)."""
    body = [line for line in pem.splitlines() if not line.startswith(b"-----")]
    der = base64.b64decode(b"".join(body))
    _, at, _ = _tls._read(der, 0)  # PrivateKeyInfo
    _, _, at = _tls._read(der, at)  # version
    _, _, at = _tls._read(der, at)  # privateKeyAlgorithm
    _, at, _ = _tls._read(der, at)  # privateKey
    _, at, _ = _tls._read(der, at)  # ECPrivateKey
    _, _, at = _tls._read(der, at)  # version
    tag, start, end = _tls._read(der, at)  # privateKey
    assert tag == 0x04
    return end - start


def p256_key(directory: Path, name: str, attempts: int = 8) -> None:
    """A P-256 key with a 32-byte scalar. LibreSSL (macOS's /usr/bin/openssl) writes 31 bytes
    1 time in 256, which rustls refuses (#216), so such a key is made again."""
    for _ in range(attempts):
        openssl("genpkey", "-algorithm", "EC", *P256, "-out", name, cwd=directory)
        if scalar_length((directory / name).read_bytes()) == 32:
            return
    raise AssertionError(f"{name}: {attempts} short P-256 keys in a row (#216)")
```

The walk above was run against LibreSSL short keys (31), LibreSSL `req` keys (31), Node keys, an OpenSSL-3-padded key (32), and synthesized keys.

- **`self_signed`** calls `p256_key(directory, key)` in place of `genpkey` (`:195-199`). Its `exists()` reuse stays.
- **`test_remote_ca_verified`** makes `ca.key` and `leaf.key` with `p256_key`. It then runs `req -new -x509 -config ca.cnf -key ca.key …` and `req -new -key leaf.key …`, as in TypeScript.
- **`P256`** stays, now used only by `p256_key`.
- **Eight attempts:** eight short keys in a row happen 1 time in 2⁶⁴. The `AssertionError` is for a broken `openssl`, not for chance.
- **Testing the retry.** The test needs a way to make the first key short. Either give `p256_key` the generator as a parameter, defaulting to the `genpkey` call, or monkeypatch the module's `openssl`. The implementer picks one.

Weighed and turned down (the user agreed; see "Decisions by the user"):

- **`cryptography` as a dev dependency.** It is deterministic and mirrors TypeScript. But it adds a compiled package and a Dependabot stream to the SDK's test toolchain for four keys. It is not on the Mac today: the lock has it only on Linux.
- **Re-encoding a short key.** LibreSSL cannot pad it (`openssl ec` keeps 31 bytes), and OpenSSL 3 is not on the self-hosted runner's PATH. Padding in Python needs a DER writer with long-form lengths, which is more code than the retry.

### The CA key

The CA key never reaches rustls. Only `openssl x509 -CAkey` reads it, and LibreSSL reads its own short keys. Clients receive `ca.pem` only. So a short CA key would not flake. It goes through the helper anyway, so that `-newkey` and `genpkey` leave the TypeScript file entirely, and `genpkey` stays in the Python file only inside `p256_key`. That makes "no openssl-made key reaches the daemon" a grep, which acceptance uses.

### Comments in the TypeScript unit tests

`tests/unit/tls.test.ts:16-17` and `pinning.test.ts:19-20` say that the named-curve parameters are what "the integration tests need". After this change the TypeScript integration tests make no openssl keys, so the comment becomes: "Named-curve parameters: LibreSSL (macOS's /usr/bin/openssl) writes explicit ones by default, and OpenSSL 3 refuses to verify a chain with such a key." That is a comment-only change. Their keys go to Node's TLS, so they keep `genpkey`. `test_tls.py`'s comment stays, since the Python integration tests still use `P256`.

### No other change

No daemon, SDK source, API, CI workflow, `justfile` or docs change.

## Deliverables

- **`sdk/typescript/tests/integration/remote.test.ts`**: `p256Key`, `scalarLength`, `selfSigned` and `remote ca verified` on them, `P256` removed, and test 1.
- **`sdk/python/tests/integration/test_remote.py`**: `scalar_length`, `p256_key`, `self_signed` and `test_remote_ca_verified` on them, and tests 2 and 3. `base64` is imported, along with `from reef import _tls`.
- **`sdk/typescript/tests/unit/tls.test.ts`** and **`pinning.test.ts`**: the comment above.
- **`CHANGELOG.md`**: none. Tests only.
- **`docs/dev-tickets/phase-2/README.md`**: the row `| #216 | [issue-216-tls-test-keys.md](issue-216-tls-test-keys.md) | S | R2.06, R2.07, R2.08 | none listed |`, after #212's (added with this brief).
- **GitHub**: the PR closes #216, and links the failed run.

## Tests

Each new case is marked `// #216 test N` or `# #216 test N`. None needs a daemon. All of them run in the integration step (`just typescript-test`, `just python-test`), which on the self-hosted Mac uses LibreSSL.

A synthesized short key, used by tests 1 to 3, is a PKCS#8 built from DER TLVs with a scalar of 31 `0x01` bytes and no public key:

```
30 40  02 01 00  30 13 06 07 2a8648ce3d0201 06 08 2a8648ce3d030107  04 26 30 24 02 01 01 04 1f <31 × 01>
```

The 32-byte variant is `30 41 … 04 27 30 25 02 01 01 04 20 <32 × 01>`. Both parse under LibreSSL and OpenSSL 3. Build them in the test with a two-line `tlv(tag, content)` helper, rather than as a hex constant, so that the structure is readable. They are not secrets: no public key, and a fixed scalar.

### TypeScript (`remote.test.ts`, its own `describe("test keys")`, outside the daemon describes)

1. **`p256Key writes a P-256 PKCS#8 key with a 32-byte scalar`**.
   - `scalarLength` reads 31 from the synthesized short key and 32 from the 32-byte one.
   - A key written by `p256Key` starts `-----BEGIN PRIVATE KEY-----`, `createPrivateKey(…).asymmetricKeyDetails?.namedCurve` is `prime256v1`, and `scalarLength` of its DER is 32.
   - `openssl req -new -x509 -key` signs it, and the certificate's public key (`X509Certificate.publicKey.export({ type: "spki", format: "der" })`) equals the key's. This runs on the leg's `openssl`, so LibreSSL on the Mac.

### Python (`test_remote.py`)

2. **`test_scalar_length`**. It reads 31 and 32 from the synthesized keys, and 32 from a key written by `p256_key`.
3. **`test_p256_key_makes_a_short_key_again`**.
   - With a stand-in generator that writes the synthesized short key first and then calls the real `genpkey`, `p256_key` calls it twice and leaves a 32-byte key.
   - With one that always writes the short key, `p256_key` raises `AssertionError` naming #216 after `attempts` calls.

   This is the retry's deterministic proof. On LibreSSL the real retry fires about 1 run in 64 per leg.

### Existing tests

4. Unchanged and passing on every leg: all of `remote.test.ts` and `test_remote.py` (now on the helpers), `local.test.ts`, `test_local.py`, and the SDKs' unit tests. `crates/reef-daemon/tests/remote.rs` is untouched.

## Implementation notes

- **Order:** TypeScript (helper, callers, test 1, then `npm run lint` and `typecheck`), then Python (helper, callers, tests 2 and 3, then `just python-check` for ruff and mypy), then the unit-test comments.
- **Lint.** `tests/**` ignores `D` and `ANN` in ruff, but `E501` (100 columns) and `RUF` apply. Keep the `# fmt: skip` style the file uses for argument-list `openssl` calls. The TypeScript ESLint config may flag a non-null assertion in the DER walk, so index with checks.
- **Run with LibreSSL locally.** Do not put `/usr/bin` first on PATH: that also swaps `git`, `python3` and others. Use a directory holding only `openssl`:

  ```sh
  shim=$(mktemp -d) && ln -s /usr/bin/openssl "$shim/openssl"
  PATH="$shim:$PATH" openssl version   # LibreSSL 3.3.6
  ```

## Acceptance criteria

- [ ] No key the daemon loads in the SDKs' remote tests comes from `openssl` unchecked. `grep -nE 'genpkey|-newkey' sdk/typescript/tests/integration/remote.test.ts` finds nothing, and in `sdk/python/tests/integration/test_remote.py` its one `openssl genpkey` call is the default generator `genpkey()`, which only `p256_key` (and test 3, through it) calls.
- [ ] Both helpers check that every key they write has a 32-byte scalar, and Python's makes a short key again (tests 1-3).
- [ ] `remote ca verified` / `test_remote_ca_verified` sign Node or helper keys with `openssl req -key` and `x509 -req`, and pass under LibreSSL locally (see "Verification").
- [ ] `just typescript-check`, `just typescript-test`, `just python-check` and `just python-test` pass, with Homebrew's `openssl` and with the LibreSSL shim.
- [ ] The PR's self-hosted `typescript` and `python` macOS legs pass on their first run.

## Verification

```sh
cargo build -p reef-vcs -p reef-daemon
just typescript-check && just typescript-test
just python-check && just python-test

# With LibreSSL, as the self-hosted runner has it (the shim above)
PATH="$shim:$PATH" just typescript-test tests/integration/remote.test.ts
PATH="$shim:$PATH" just python-test tests/integration/test_remote.py
```

Then loop each of the last two commands 25 times under the shim and report the counts. Before the fix, a run failed 1 time in 64, so 25 clean runs would happen by chance about 67% of the time. The loop shows that nothing else broke. It does not prove the fix. The deterministic evidence is:

- The grep in "Acceptance criteria".
- Tests 1-3.
- The negative check, repeated once by hand and reported in the PR. Find a short key with `/usr/bin/openssl genpkey` in a loop, checking its DER for `04 1f` (about 1 in 256), give it and a certificate to a scratch daemon, and see "do not make a usable pair". Re-encode it with Homebrew's OpenSSL 3 (`openssl pkey -in short.pem -out padded.pem`), and see "remote listener serving". This shows that the scalar's length is the whole cause.

## Out of scope

- **The daemon padding a short scalar**, and **a docs warning for macOS users**. The user chose a follow-up issue for the padding (see "Decisions by the user").
- **The SDKs' unit tests' keys.** Node's TLS and Python's `ssl` accept a 31-byte scalar (verified above). They are unchanged apart from the comment.
- **Moving the tests off `openssl` entirely** (certificates from Node or `cryptography`). The certificates are not the problem.
- **The self-hosted runner's PATH.** Adding Homebrew's `openssl` would hide the issue on one machine, not fix the tests, and #211 keeps Homebrew off PATH on purpose.

## Decisions by the user (2026-11-05)

These were the brief's open questions.

1. **The daemon and short scalars:** this ticket stays tests-only. A follow-up issue (Phase 3) asks the daemon to left-pad a short P-256/P-384 scalar in `load_pair` (`remote/tls.rs`), as described below. With that fix planned, the quickstart gets no warning.
2. **Who files it:** the implementer files the follow-up issue with the PR and links it there. Filed as #219.
3. **Python:** make the key again if it is short (the design above). No `cryptography` dev dependency.

### The daemon fix, for the follow-up issue

A user on macOS who makes a key with the stock `openssl` hits this 1 time in 256. They see "failed to parse private key as RSA, ECDSA, or EdDSA", which does not say what is wrong. A daemon fix belongs in `load_pair` (`remote/tls.rs:268-292`), between `PrivateKeyDer::from_pem_slice` and `CertifiedKey::from_der`:
- For a PKCS#8 (or SEC1) EC key on P-256 or P-384 whose `privateKey` is shorter than the field (32 or 48 bytes), left-pad it with zeros and re-encode the two or three enclosing lengths.
- `tls.rs`'s `der` module (`:439` on) already has a TLV reader (`tlv`, `expect`). The writer, with long-form lengths, is new: about 50 to 80 lines.
- Tests: unit tests on a synthesized short key, and a `tests/remote.rs` case shaped like `supplied_certificate` with a 31-byte key. Plus a CHANGELOG **Fixed** line.
- Padding is enough: the padded encoding of the same scalar serves (verified above).
- Switching the key parser to aws-lc-rs is not an option: `crates/reef-testing/tests/crate_graph.rs:64` bans it.
- A cheaper middle way: detect the short scalar and fail with a message that names it and how to fix it. LibreSSL cannot fix it, though (`openssl ec` keeps 31 bytes), so the message would have to point to OpenSSL 3 or to making the key again.

Each is S on its own, and none is needed for this flake.
