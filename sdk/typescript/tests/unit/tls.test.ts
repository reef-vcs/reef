// Fingerprints, names and dates of certificates openssl makes in the test (test 5).

import { execFileSync } from "node:child_process";
import { createHash, X509Certificate } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import type { AddressInfo } from "node:net";
import { join } from "node:path";
import { connect as tlsConnect, createServer, type PeerCertificate } from "node:tls";

import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { RemoteTlsError } from "../../src/errors.js";
import { checkDates, fingerprintOf, parseLeaf, pemOf, targetName } from "../../src/tls.js";

// Named-curve parameters, as the integration tests need (LibreSSL, macOS's /usr/bin/openssl,
// writes explicit ones by default).
const P256 = ["-pkeyopt", "ec_paramgen_curve:P-256", "-pkeyopt", "ec_param_enc:named_curve"];

let dir: string;
beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "reef-tls-"));
});
afterAll(() => {
  rmSync(dir, { recursive: true, force: true });
});

function openssl(...args: string[]): Buffer {
  return execFileSync("openssl", args, { cwd: dir, stdio: ["ignore", "pipe", "ignore"] });
}

function cert(name: string, keyArgs: string[], san: string, days = "30"): Buffer {
  openssl("genpkey", ...keyArgs, "-out", `${name}.key`);
  openssl(
    "req",
    "-new",
    "-x509",
    "-key",
    `${name}.key`,
    "-out",
    `${name}.pem`,
    "-days",
    days,
    "-subj",
    "/CN=reefvm.test",
    "-addext",
    `subjectAltName=${san}`,
  );
  return new X509Certificate(readFileSync(join(dir, `${name}.pem`))).raw;
}

function opensslFingerprint(name: string): string {
  const der = openssl("pkey", "-in", `${name}.key`, "-pubout", "-outform", "DER");
  return "sha256:" + createHash("sha256").update(der).digest("hex");
}

describe("tls", () => {
  it("fingerprints the SubjectPublicKeyInfo, for EC and RSA keys", () => {
    const ec = cert("ec", ["-algorithm", "EC", ...P256], "DNS:reefvm.test");
    const rsa = cert(
      "rsa",
      ["-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048"],
      "DNS:reefvm.test",
    );
    expect(fingerprintOf(ec)).toBe(opensslFingerprint("ec"));
    expect(fingerprintOf(rsa)).toBe(opensslFingerprint("rsa"));
    expect(fingerprintOf(rsa)).not.toBe(fingerprintOf(ec));
  });

  it("never hashes getPeerCertificate().pubkey: for an EC key it is the bare point", async () => {
    const der = cert("trap", ["-algorithm", "EC", ...P256], "DNS:reefvm.test");
    const server = createServer({
      key: readFileSync(join(dir, "trap.key")),
      cert: readFileSync(join(dir, "trap.pem")),
    });
    await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
    const { port } = server.address() as AddressInfo;
    const peer = await new Promise<PeerCertificate>((done, fail) => {
      const socket = tlsConnect({ host: "127.0.0.1", port, rejectUnauthorized: false }, () => {
        done(socket.getPeerCertificate(true));
        socket.destroy();
      });
      socket.once("error", fail);
    });
    await new Promise((done) => server.close(done));
    expect(peer.raw.equals(der)).toBe(true);
    expect(fingerprintOf(peer.raw)).toBe(opensslFingerprint("trap"));
    expect(peer.pubkey).toBeDefined();
    const pubkey =
      "sha256:" +
      createHash("sha256")
        .update(peer.pubkey ?? Buffer.alloc(0))
        .digest("hex");
    expect(pubkey).not.toBe(fingerprintOf(peer.raw));
  });

  it("chooses the name the session uses", () => {
    const der = cert(
      "names",
      ["-algorithm", "EC", ...P256],
      "DNS:reefvm.test,DNS:other.test,IP:127.0.0.1",
    );
    const leaf = parseLeaf(der);
    expect(leaf.names).toEqual(["reefvm.test", "other.test", "127.0.0.1"]);
    expect(targetName(leaf, "other.test")).toBe("other.test");
    expect(targetName(leaf, "127.0.0.1")).toBe("127.0.0.1");
    expect(targetName(leaf, "localhost")).toBe("reefvm.test");
    expect(targetName(leaf, "10.0.0.1")).toBe("reefvm.test");
    const ipOnly = parseLeaf(cert("ip", ["-algorithm", "EC", ...P256], "IP:10.1.2.3"));
    expect(targetName(ipOnly, "vm")).toBe("10.1.2.3");
    expect(pemOf(der)).toContain("-----BEGIN CERTIFICATE-----");
    expect(new X509Certificate(pemOf(der)).raw.equals(der)).toBe(true);
  });

  it("refuses a leaf outside its dates", () => {
    const leaf = parseLeaf(cert("dates", ["-algorithm", "EC", ...P256], "DNS:reefvm.test"));
    const late = new Date(leaf.notAfter.getTime() + 1_000);
    const early = new Date(leaf.notBefore.getTime() - 1_000);
    for (const when of [late, early]) {
      let thrown: unknown;
      try {
        checkDates(leaf, "https://vm:1", when);
      } catch (error) {
        thrown = error;
      }
      expect(thrown).toBeInstanceOf(RemoteTlsError);
      expect((thrown as RemoteTlsError).failure).toBe("handshake");
    }
    expect(() => {
      checkDates(leaf, "https://vm:1", new Date());
    }).not.toThrow();
  });
});
