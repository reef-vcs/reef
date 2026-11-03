// A pinned or CA-verified session writes nothing to a server without the trusted key, whatever
// NODE_TLS_REJECT_UNAUTHORIZED says (review finding: Node skips checkServerIdentity when the chain
// check is ignored).

import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer, type Server } from "node:tls";

import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";

import { DaemonService } from "../../src/gen/reef/api/v1/daemon_pb.js";
import { RemoteUrl, type RemoteTarget } from "../../src/remote.js";
import { parseLeaf } from "../../src/tls.js";
import { caSession, pinnedSession, type Session } from "../../src/transport.js";

// Named-curve parameters, as the integration tests need (LibreSSL, macOS's /usr/bin/openssl,
// writes explicit ones by default).
const P256 = ["-pkeyopt", "ec_paramgen_curve:P-256", "-pkeyopt", "ec_param_enc:named_curve"];

let dir: string;
let server: Server;
let port = 0;
let received = 0;
let trusted: Buffer;

function openssl(...args: string[]): void {
  execFileSync("openssl", args, { cwd: dir, stdio: "ignore" });
}

function selfSigned(name: string): void {
  openssl("genpkey", "-algorithm", "EC", ...P256, "-out", `${name}.key`);
  openssl(
    "req",
    "-new",
    "-x509",
    "-key",
    `${name}.key`,
    "-out",
    `${name}.pem`,
    "-days",
    "30",
    "-subj",
    "/CN=reefvm.test",
    "-addext",
    "subjectAltName=DNS:reefvm.test,IP:127.0.0.1",
  );
}

beforeAll(async () => {
  dir = mkdtempSync(join(tmpdir(), "reef-pin-"));
  selfSigned("trusted");
  selfSigned("impostor");
  trusted = Buffer.from(
    readFileSync(join(dir, "trusted.pem"), "utf8").replace(/-----[^-]+-----|\s/g, ""),
    "base64",
  );
  // The impostor holds another key; it counts every byte a client writes after the handshake.
  server = createServer({
    key: readFileSync(join(dir, "impostor.key")),
    cert: readFileSync(join(dir, "impostor.pem")),
    ALPNProtocols: ["h2"],
  });
  server.on("secureConnection", (socket) => {
    socket.on("data", (data: Buffer) => {
      received += data.length;
    });
    socket.on("error", () => undefined);
  });
  server.on("tlsClientError", () => undefined);
  await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
  port = (server.address() as AddressInfo).port;
});

afterAll(async () => {
  await new Promise((done) => server.close(done));
  rmSync(dir, { recursive: true, force: true });
});

const saved = process.env["NODE_TLS_REJECT_UNAUTHORIZED"];
afterEach(() => {
  if (saved === undefined) {
    delete process.env["NODE_TLS_REJECT_UNAUTHORIZED"];
  } else {
    process.env["NODE_TLS_REJECT_UNAUTHORIZED"] = saved;
  }
});

function target(fingerprint: string | undefined): RemoteTarget {
  return { url: new RemoteUrl("127.0.0.1", port), token: "secret", fingerprint, name: undefined };
}

async function call(session: Session): Promise<unknown> {
  try {
    await session.transport.unary(
      DaemonService.method.getDaemonStatus,
      undefined,
      2_000,
      { authorization: "Bearer secret" },
      {},
    );
    return undefined;
  } catch (error) {
    return error;
  } finally {
    session.manager.abort();
  }
}

describe.each([
  ["by default", undefined],
  ["with NODE_TLS_REJECT_UNAUTHORIZED=0", "0"],
])("a server without the trusted key, %s", (_name, setting) => {
  it("gets nothing from a pinned session", async () => {
    if (setting !== undefined) {
      process.env["NODE_TLS_REJECT_UNAUTHORIZED"] = setting;
    }
    received = 0;
    const leaf = parseLeaf(trusted);
    const error = await call(pinnedSession(target(leaf.fingerprint), leaf, 2_000));
    expect(error).toBeDefined();
    expect(received).toBe(0);
  });

  it("gets nothing from a CA-verified session", async () => {
    if (setting !== undefined) {
      process.env["NODE_TLS_REJECT_UNAUTHORIZED"] = setting;
    }
    received = 0;
    const error = await call(
      caSession(target(undefined), [readFileSync(join(dir, "trusted.pem"), "utf8")], 2_000),
    );
    expect(error).toBeDefined();
    expect(received).toBe(0);
  });
});
