// The SDK against a daemon's remote listener (R2.08): pinned, by profile, CA-verified, renewed
// (R2.07's tests 25 to 31).

import { execFileSync } from "node:child_process";
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

function selfSigned(
  dir: string,
  key: string,
  cert: string,
  days = "30",
  names = "DNS:reefvm.test",
): void {
  if (!existsSync(join(dir, key))) {
    openssl(dir, "genpkey", "-algorithm", "EC", "-pkeyopt", "ec_paramgen_curve:P-256", "-out", key);
  }
  openssl(
    dir,
    "req",
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
    openssl(
      tls,
      "req",
      "-x509",
      "-newkey",
      "ec",
      "-pkeyopt",
      "ec_paramgen_curve:P-256",
      "-nodes",
      "-keyout",
      "ca.key",
      "-out",
      "ca.pem",
      "-days",
      "30",
      "-subj",
      "/CN=reef test CA",
      "-addext",
      "basicConstraints=critical,CA:TRUE",
      "-addext",
      "keyUsage=critical,keyCertSign",
    );
    openssl(
      tls,
      "req",
      "-new",
      "-newkey",
      "ec",
      "-pkeyopt",
      "ec_paramgen_curve:P-256",
      "-nodes",
      "-keyout",
      "leaf.key",
      "-out",
      "leaf.csr",
      "-subj",
      "/CN=reefvm.test",
    );
    writeFileSync(
      join(tls, "ext.cnf"),
      "subjectAltName=DNS:reefvm.test,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n",
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
