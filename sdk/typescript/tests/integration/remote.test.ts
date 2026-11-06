// The SDK against a daemon's remote listener (R2.08): pinned, by profile, CA-verified, renewed
// (R2.07's tests 25 to 31).

import { execFileSync } from "node:child_process";
import {
  createPrivateKey,
  createPublicKey,
  generateKeyPairSync,
  X509Certificate,
} from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";

import { IfNotJj, RemoteScope, WorkspaceState } from "../../src/api/v1.js";
import {
  API_REVISION,
  connect,
  PermissionDeniedError,
  RemoteMisdirectedError,
  RemoteTlsError,
  UnavailableError,
} from "../../src/index.js";
import { BASE_CONFIG, binDir, Daemon, freePort, makeRepo } from "./harness.js";

// The test CA's own configuration, not the system's: OpenSSL 1.1.1 (Homebrew's openssl@1.1, first
// on PATH on the macOS runners) applies the system file's v3_ca section and appends `-addext`'s
// copy, and OpenSSL 3 refuses a CA with two basicConstraints.
const CA_CONFIG = `[req]
distinguished_name = dn
prompt = no
x509_extensions = ca
[dn]
CN = reef test CA
[ca]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign
subjectKeyIdentifier = hash
`;

function remoteConfig(listen: string, extra = ""): string {
  return `${BASE_CONFIG}\n[remote]\nenabled = true\nlisten = "${listen}"\nnames = ["reefvm.test"]\n${extra}`;
}

/** A daemon with its remote listener, a repository, and write and read tokens. */
class Remote {
  readonly daemon: Daemon;
  address = "";
  fingerprint = "";
  repo = "";
  write = "";
  read = "";

  constructor(daemon: Daemon) {
    this.daemon = daemon;
  }

  async setUp(): Promise<this> {
    const admin = await this.daemon.client();
    try {
      const status = await admin.getDaemonStatus({ skipBackends: true });
      this.address = status.remote?.address ?? "";
      this.fingerprint = status.remote?.certificate?.fingerprint ?? "";
      const path = makeRepo(this.daemon, "remote repo");
      this.repo =
        (await admin.registerRepository({ path, ifNotJj: IfNotJj.INIT_ALONGSIDE_GIT })).repository
          ?.id ?? "";
      this.write = (
        await admin.createRemoteToken({
          name: "sdk-write",
          scope: RemoteScope.WRITE,
          neverExpires: true,
        })
      ).secret;
      this.read = (
        await admin.createRemoteToken({
          name: "sdk-read",
          scope: RemoteScope.READ,
          neverExpires: true,
        })
      ).secret;
    } finally {
      await admin.close();
    }
    return this;
  }

  get url(): string {
    return `https://${this.address}`;
  }

  get port(): string {
    return this.address.slice(this.address.lastIndexOf(":") + 1);
  }

  async token(name: string) {
    const admin = await this.daemon.client();
    try {
      for await (const token of admin.listRemoteTokensAll({})) {
        if (token.name === name) {
          return token;
        }
      }
      throw new Error(`no token ${name}`);
    } finally {
      await admin.close();
    }
  }
}

const REMOTE_ENV = ["REEF_REMOTE", "REEF_REMOTE_TOKEN", "REEF_REMOTE_FINGERPRINT", "REEF_SOCKET"];
let saved: NodeJS.ProcessEnv;
beforeEach(() => {
  saved = { ...process.env };
  for (const name of REMOTE_ENV) {
    Reflect.deleteProperty(process.env, name);
  }
});
afterEach(() => {
  process.env = saved;
});

function openssl(cwd: string, ...args: string[]): void {
  execFileSync("openssl", args, { cwd, stdio: "ignore" });
}

/** The length of a PKCS#8 EC key's private scalar (RFC 5915: 32 bytes for P-256). */
function scalarLength(pkcs8: Buffer): number {
  let at = 0;
  // One DER element at `at`: its tag, and where its content starts and ends.
  const read = (): { tag: number; start: number; end: number } => {
    const tag = pkcs8[at];
    let length = pkcs8[at + 1];
    let start = at + 2;
    if (tag === undefined || length === undefined) throw new Error("truncated DER");
    if (length & 0x80) {
      const count = length & 0x7f;
      if (count === 0 || count > 4) throw new Error(`bad DER length: ${count} bytes`);
      length = 0;
      for (let i = 0; i < count; i++) {
        const byte = pkcs8[start + i];
        if (byte === undefined) throw new Error("truncated DER");
        length = length * 256 + byte;
      }
      start += count;
    }
    if (start + length > pkcs8.length) throw new Error("truncated DER");
    return { tag, start, end: start + length };
  };
  const enter = (): void => {
    at = read().start;
  };
  const skip = (): void => {
    at = read().end;
  };
  enter(); // PrivateKeyInfo
  skip(); // version
  skip(); // privateKeyAlgorithm
  enter(); // privateKey
  enter(); // ECPrivateKey
  skip(); // version
  const scalar = read(); // privateKey
  if (scalar.tag !== 0x04) throw new Error(`not an OCTET STRING: ${scalar.tag}`);
  return scalar.end - scalar.start;
}

/** A P-256 key, PKCS#8 PEM, made by Node. LibreSSL (macOS's /usr/bin/openssl) writes a 31-byte
 * scalar 1 time in 256, which rustls refuses, so no key the daemon loads comes from openssl
 * (#216). */
function p256Key(path: string): void {
  const { privateKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const length = scalarLength(privateKey.export({ type: "pkcs8", format: "der" }));
  if (length !== 32) {
    throw new Error(`${path}: a ${length}-byte P-256 scalar (#216)`);
  }
  writeFileSync(path, privateKey.export({ type: "pkcs8", format: "pem" }), { mode: 0o600 });
}

function selfSigned(
  dir: string,
  key: string,
  cert: string,
  days = "30",
  names = "DNS:reefvm.test",
): void {
  if (!existsSync(join(dir, key))) {
    p256Key(join(dir, key));
  }
  openssl(
    dir,
    "req",
    "-new",
    "-x509",
    "-key",
    key,
    "-out",
    cert,
    "-days",
    days,
    "-subj",
    "/CN=reefvm.test",
    "-addext",
    `subjectAltName=${names},IP:127.0.0.1`,
  );
}

function shortTmp(): string {
  return mkdtempSync(join(tmpdir(), "reef-"));
}

describe("remote daemon", () => {
  let remote: Remote;

  beforeAll(async () => {
    const daemon = new Daemon(remoteConfig("127.0.0.1:0"));
    await daemon.start();
    remote = await new Remote(daemon).setUp();
  });

  afterAll(async () => {
    await remote.daemon.remove();
  });

  // 25
  it("remote pinned by environment", async () => {
    process.env["REEF_REMOTE"] = remote.url;
    process.env["REEF_REMOTE_TOKEN"] = remote.write;
    process.env["REEF_REMOTE_FINGERPRINT"] = remote.fingerprint;
    const client = await connect({ agent: "remote-sdk" });
    try {
      expect(client.remote).toBe(true);
      expect(client.target).toBe(remote.url);
      expect((await client.daemonStatus()).apiRevision).toBe(API_REVISION);
      for await (const _ of client.listWorkspacesAll({ repo: remote.repo })) {
        // Every page.
      }
      let change = "";
      await client.withWorkspace(
        { repo: remote.repo, purpose: "over the wire", reportPresence: false },
        async (ws) => {
          const edited = await ws.editFiles({
            edits: [
              { path: "remote.txt", action: { case: "content", value: Buffer.from("remote\n") } },
            ],
          });
          change = edited.change?.changeId ?? ws.changeId;
        },
      );
      const read = await client.readFile({
        repo: remote.repo,
        revision: change,
        path: "remote.txt",
      });
      expect(Buffer.from(read.content).toString()).toBe("remote\n");
      const ops = [];
      for await (const op of client.listOperationsAll({ repo: remote.repo, pageSize: 5 })) {
        ops.push(op);
      }
      expect(ops.some((op) => op.remote !== undefined)).toBe(true);
    } finally {
      await client.close();
    }
  });

  // 26
  it("remote profile", async () => {
    const dir = shortTmp();
    try {
      const state = join(dir, "client");
      execFileSync(
        join(binDir(), "reef"),
        [
          "remote",
          "add",
          "vm",
          remote.url,
          "--fingerprint",
          remote.fingerprint,
          "--token-file",
          "-",
        ],
        {
          input: remote.read + "\n",
          env: { ...remote.daemon.env, REEF_STATE_DIR: state },
          stdio: ["pipe", "ignore", "ignore"],
        },
      );
      const client = await connect({ remote: "vm", stateDir: state });
      expect(client.target).toBe(remote.url);
      await client.getDaemonStatus({ skipBackends: true });
      await client.close();
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  // 27
  it("remote pin mismatch sends no token", async () => {
    const admin = await remote.daemon.client();
    const secret = (
      await admin.createRemoteToken({
        name: "sdk-unused",
        scope: RemoteScope.READ,
        neverExpires: true,
      })
    ).secret;
    await admin.close();
    const wrong = "sha256:" + "0".repeat(64);
    const error: unknown = await connect({
      remote: remote.url,
      token: secret,
      fingerprint: wrong,
    }).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(RemoteTlsError);
    expect(error).toMatchObject({ failure: "pin_mismatch", presented: remote.fingerprint });
    expect((error as RemoteTlsError).hint).toContain(remote.fingerprint);
    expect((await remote.token("sdk-unused")).lastUsedAt).toBeUndefined();
  });

  // 28
  it("remote scopes and errors", async () => {
    const reader = await connect({
      remote: remote.url,
      token: remote.read,
      fingerprint: remote.fingerprint,
    });
    try {
      await reader.listWorkspaces({ repo: remote.repo });
      const refused: unknown = await reader
        .createWorkspace({ repo: remote.repo, owner: "x@y" })
        .catch((e: unknown) => e);
      expect(refused).toBeInstanceOf(PermissionDeniedError);
      expect((refused as PermissionDeniedError).reason).toBe("REMOTE_ACCESS_DENIED");
      await expect(reader.listRemoteTokens({})).rejects.toBeInstanceOf(PermissionDeniedError);
      // A unary and a streaming service error over the listener arrive with their detail.
      await expect(reader.getWorkspace({ workspace: "0000000000000000" })).rejects.toMatchObject({
        reason: "WORKSPACE_NOT_FOUND",
      });
      const stream = reader.subscribeOperations({ repos: ["ffffffffffffffff"] });
      await expect(stream.next()).rejects.toMatchObject({ reason: "REPOSITORY_NOT_FOUND" });
    } finally {
      await reader.close();
    }
    const wrong = await connect({
      remote: remote.url,
      token: remote.read,
      fingerprint: remote.fingerprint,
    });
    await wrong.close();
    const unknown: unknown = await connect({
      remote: remote.url,
      token: "reef_wrong",
      fingerprint: remote.fingerprint,
    }).catch((e: unknown) => e);
    expect(unknown).toBeInstanceOf(PermissionDeniedError);
    expect((unknown as PermissionDeniedError).reason).toBe("REMOTE_TOKEN_INVALID");

    const writer = await connect({
      remote: remote.url,
      token: remote.write,
      fingerprint: remote.fingerprint,
    });
    try {
      await writer.withWorkspace({ repo: remote.repo, reportPresence: true }, async (ws) => {
        await ws.editFiles({
          edits: [{ path: "w.txt", action: { case: "content", value: Buffer.from("w\n") } }],
        });
        const read = await ws.readFile("w.txt");
        expect(Buffer.from(read.content).toString()).toBe("w\n");
      });
    } finally {
      await writer.close();
    }
  });

  // 29, the first half
  it("remote host not in the certificate", async () => {
    // `localhost` is neither a name of the generated certificate nor in `[remote] names`: pinned,
    // the SDK names the certificate's own name to TLS and the daemon (ADR 0054 decision 6).
    const url = `https://localhost:${remote.port}`;
    const client = await connect({
      remote: url,
      token: remote.read,
      fingerprint: remote.fingerprint,
    });
    expect(client.target).toBe(url);
    await client.getDaemonStatus({ skipBackends: true });
    await client.close();
  });
});

describe("remote daemon with supplied certificates", () => {
  let dir: string;
  beforeEach(() => {
    dir = shortTmp();
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  // 29, the second half
  it("remote misdirected", async () => {
    const tls = join(dir, "tls");
    mkdirSync(tls);
    selfSigned(tls, "key.pem", "cert.pem", "30", "DNS:reefvm.test,DNS:localhost");
    const extra = `tls_cert = "${join(tls, "cert.pem")}"\ntls_key = "${join(tls, "key.pem")}"\n`;
    const daemon = new Daemon(remoteConfig("127.0.0.1:0", extra));
    await daemon.start();
    try {
      const remote = await new Remote(daemon).setUp();
      const error: unknown = await connect({
        remote: `https://localhost:${remote.port}`,
        token: remote.read,
        fingerprint: remote.fingerprint,
      }).catch((e: unknown) => e);
      expect(error).toBeInstanceOf(RemoteMisdirectedError);
      expect((error as RemoteMisdirectedError).host).toBe("localhost");
    } finally {
      await daemon.remove();
    }
  });

  // 30
  it("remote ca verified", async () => {
    const tls = join(dir, "tls");
    mkdirSync(tls);
    writeFileSync(join(tls, "ca.cnf"), CA_CONFIG);
    p256Key(join(tls, "ca.key"));
    p256Key(join(tls, "leaf.key"));
    openssl(
      tls,
      "req",
      "-new",
      "-x509",
      "-config",
      "ca.cnf",
      "-key",
      "ca.key",
      "-out",
      "ca.pem",
      "-days",
      "30",
      "-sha256",
    );
    openssl(tls, "req", "-new", "-key", "leaf.key", "-out", "leaf.csr", "-subj", "/CN=reefvm.test");
    writeFileSync(
      join(tls, "ext.cnf"),
      // OpenSSL 1.1.1 and LibreSSL add no AKID, which strict verifiers (Python 3.13+) require.
      "subjectAltName=DNS:reefvm.test,IP:127.0.0.1\nextendedKeyUsage=serverAuth\nauthorityKeyIdentifier=keyid\n",
    );
    openssl(
      tls,
      "x509",
      "-req",
      "-in",
      "leaf.csr",
      "-CA",
      "ca.pem",
      "-CAkey",
      "ca.key",
      "-CAcreateserial",
      "-out",
      "leaf.pem",
      "-days",
      "30",
      "-sha256",
      "-extfile",
      "ext.cnf",
    );
    const extra = `tls_cert = "${join(tls, "leaf.pem")}"\ntls_key = "${join(tls, "leaf.key")}"\n`;
    const daemon = new Daemon(remoteConfig("127.0.0.1:0", extra));
    await daemon.start();
    try {
      const remote = await new Remote(daemon).setUp();
      const ca = readFileSync(join(tls, "ca.pem"), "utf8");
      const client = await connect({
        remote: remote.url,
        token: remote.read,
        rootCertificates: [ca],
      });
      await client.getDaemonStatus({ skipBackends: true });
      await client.close();
      const error: unknown = await connect({ remote: remote.url, token: remote.read }).catch(
        (e: unknown) => e,
      );
      expect(error).toBeInstanceOf(RemoteTlsError);
      expect((error as RemoteTlsError).failure).toBe("untrusted");
      expect((error as RemoteTlsError).presented).toMatch(/^sha256:/);
    } finally {
      await daemon.remove();
    }
  });

  // 31
  it("remote repins after renewal", async () => {
    const tls = join(dir, "tls");
    mkdirSync(tls);
    selfSigned(tls, "key.pem", "cert1.pem");
    copyFileSync(join(tls, "cert1.pem"), join(tls, "cert.pem"));
    const port = await freePort();
    const extra = `tls_cert = "${join(tls, "cert.pem")}"\ntls_key = "${join(tls, "key.pem")}"\n`;
    const daemon = new Daemon(remoteConfig(`127.0.0.1:${port}`, extra));
    await daemon.start();
    try {
      const remote = await new Remote(daemon).setUp();
      const client = await connect({
        remote: remote.url,
        token: remote.write,
        fingerprint: remote.fingerprint,
      });
      await client.getDaemonStatus({ skipBackends: true });

      // A stream open across a restart ends with UnavailableError and is not reopened.
      const open = client.subscribeOperations({});
      const iterator = open[Symbol.asyncIterator]();
      const first = iterator.next();
      await new Promise((done) => setTimeout(done, 300));

      // A renewal: a new certificate for the same key.
      selfSigned(tls, "key.pem", "cert2.pem", "31");
      copyFileSync(join(tls, "cert2.pem"), join(tls, "cert.pem"));
      await daemon.stop();
      await expect(first.then(() => iterator.next())).rejects.toBeInstanceOf(UnavailableError);
      await daemon.start();

      // The handshake refused the old leaf before anything was sent, so a read and a write are
      // each retried once on a re-pinned session.
      const status = await client.getDaemonStatus({ skipBackends: true });
      expect(status.remote?.certificate?.fingerprint).toBe(remote.fingerprint);

      selfSigned(tls, "key.pem", "cert4.pem", "32");
      copyFileSync(join(tls, "cert4.pem"), join(tls, "cert.pem"));
      await daemon.stop();
      await daemon.start();
      const made = await client.createWorkspace({ repo: remote.repo, owner: "renewal@test" });
      expect(made.workspace?.state).toBe(WorkspaceState.ACTIVE);
      await client.releaseWorkspace({ workspace: made.workspace?.id ?? "" });

      // A new key: refused before anything is sent.
      unlinkSync(join(tls, "key.pem"));
      selfSigned(tls, "key.pem", "cert3.pem");
      copyFileSync(join(tls, "cert3.pem"), join(tls, "cert.pem"));
      await daemon.stop();
      await daemon.start();
      const error: unknown = await client
        .getDaemonStatus({ skipBackends: true })
        .catch((e: unknown) => e);
      expect(error).toBeInstanceOf(RemoteTlsError);
      expect((error as RemoteTlsError).failure).toBe("pin_mismatch");
      await client.close();
    } finally {
      await daemon.remove();
    }
  });
});

describe("test keys", () => {
  /** One short-form DER element. */
  function tlv(tag: number, ...content: Buffer[]): Buffer {
    const body = Buffer.concat(content);
    return Buffer.concat([Buffer.from([tag, body.length]), body]);
  }

  /** A PKCS#8 P-256 key around a scalar of `length` 0x01 bytes, with no public key. */
  function syntheticKey(length: number): Buffer {
    const ecPublicKey = Buffer.from("2a8648ce3d0201", "hex");
    const prime256v1 = Buffer.from("2a8648ce3d030107", "hex");
    const ecPrivateKey = tlv(0x30, tlv(0x02, Buffer.from([1])), tlv(0x04, Buffer.alloc(length, 1)));
    return tlv(
      0x30,
      tlv(0x02, Buffer.from([0])),
      tlv(0x30, tlv(0x06, ecPublicKey), tlv(0x06, prime256v1)),
      tlv(0x04, ecPrivateKey),
    );
  }

  let dir: string;
  beforeEach(() => {
    dir = shortTmp();
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  // #216 test 1
  it("p256Key writes a P-256 PKCS#8 key with a 32-byte scalar", () => {
    expect(scalarLength(syntheticKey(31))).toBe(31);
    expect(scalarLength(syntheticKey(32))).toBe(32);
    expect(() => scalarLength(syntheticKey(32).subarray(0, 40))).toThrow("truncated DER");

    p256Key(join(dir, "key.pem"));
    const pem = readFileSync(join(dir, "key.pem"), "utf8");
    expect(pem.startsWith("-----BEGIN PRIVATE KEY-----")).toBe(true);
    const key = createPrivateKey(pem);
    expect(key.asymmetricKeyDetails?.namedCurve).toBe("prime256v1");
    // The file's own bytes: Node's export of a key pads a short scalar, so it would hide one.
    const der = Buffer.from(pem.replace(/-----[^-]+-----|\s/g, ""), "base64");
    expect(scalarLength(der)).toBe(32);

    // The leg's openssl (LibreSSL on the self-hosted Mac) signs with it.
    selfSigned(dir, "key.pem", "cert.pem");
    const certificate = new X509Certificate(readFileSync(join(dir, "cert.pem")));
    expect(certificate.publicKey.export({ type: "spki", format: "der" })).toEqual(
      createPublicKey(key).export({ type: "spki", format: "der" }),
    );
  });
});
