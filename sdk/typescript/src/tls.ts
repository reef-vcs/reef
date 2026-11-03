// Pinning a remote daemon's key before anything is sent (R2.08's requirement 1, ADR 0054).
//
// The SDK reads the daemon's leaf certificate on a handshake of its own that sends nothing after
// it, hashes the leaf's SubjectPublicKeyInfo, and compares it with the pin. The session then
// trusts that certificate as its only root and checks the key again in every handshake
// (`checkServerIdentity`), so the token reaches only a server holding the pinned key, on
// reconnects too.

import { createHash, X509Certificate } from "node:crypto";
import { isIP } from "node:net";
import { connect as tlsConnect, type PeerCertificate } from "node:tls";

import { RemoteTlsError } from "./errors.js";
import type { RemoteUrl } from "./remote.js";

/** What the SDK reads from a certificate. */
export interface Leaf {
  der: Buffer;
  /** `sha256:` and the hex SHA-256 of the SubjectPublicKeyInfo: what clients pin. */
  fingerprint: string;
  /** The subjectAltName's DNS names and IP addresses, in order. */
  names: string[];
  notBefore: Date;
  notAfter: Date;
  x509: X509Certificate;
}

/**
 * The SubjectPublicKeyInfo's fingerprint. Never `getPeerCertificate().pubkey`'s: for an EC key
 * that is the bare point, whose hash matches nothing a daemon reports.
 */
export function fingerprintOf(der: Buffer | X509Certificate): string {
  const x509 = der instanceof X509Certificate ? der : new X509Certificate(der);
  const spki = x509.publicKey.export({ type: "spki", format: "der" });
  return "sha256:" + createHash("sha256").update(spki).digest("hex");
}

/** The names in a subjectAltName string, `DNS:a, IP Address:1.2.3.4`, in order. */
export function sanNames(subjectAltName: string | undefined): string[] {
  if (!subjectAltName) {
    return [];
  }
  const names: string[] = [];
  for (const entry of subjectAltName.split(/,\s*/)) {
    const colon = entry.indexOf(":");
    if (colon < 0) {
      continue;
    }
    const kind = entry.slice(0, colon);
    const value = entry.slice(colon + 1).replace(/^"|"$/g, "");
    if (kind === "DNS" || kind === "IP Address") {
      names.push(value);
    }
  }
  return names;
}

/** The key and names of a DER certificate. */
export function parseLeaf(der: Buffer): Leaf {
  const x509 = new X509Certificate(der);
  return {
    der,
    fingerprint: fingerprintOf(x509),
    names: sanNames(x509.subjectAltName),
    notBefore: new Date(x509.validFrom),
    notAfter: new Date(x509.validTo),
    x509,
  };
}

/** The certificate as PEM, for a TLS context's roots. */
export function pemOf(der: Buffer): string {
  const body = der.toString("base64").replace(/.{1,64}/g, "$&\n");
  return `-----BEGIN CERTIFICATE-----\n${body}-----END CERTIFICATE-----\n`;
}

/** Why reading the leaf failed: nothing answered, or the TLS handshake failed. */
export class LeafError extends Error {
  readonly stage: "connect" | "tls";
  constructor(stage: "connect" | "tls", message: string, cause: unknown) {
    super(message, { cause });
    this.stage = stage;
  }
}

/** Whether a Node error is a failure to reach the host, not a TLS failure. */
export function isConnectFailure(error: unknown): boolean {
  const code = codeOf(error);
  return (
    code !== undefined &&
    [
      "ECONNREFUSED",
      "ETIMEDOUT",
      "EHOSTUNREACH",
      "ENETUNREACH",
      "ENOTFOUND",
      "EAI_AGAIN",
      "ECONNRESET",
      "EPIPE",
      "ENOENT",
      "EADDRNOTAVAIL",
    ].includes(code)
  );
}

/** A Node error's `code`, if it has one. */
export function codeOf(error: unknown): string | undefined {
  if (typeof error === "object" && error !== null && "code" in error) {
    const code = error.code;
    return typeof code === "string" ? code : undefined;
  }
  return undefined;
}

/** SNI for a host: its name, never an IP address (Node warns on one). */
export function serverName(host: string): string | undefined {
  return isIP(host) === 0 ? host : undefined;
}

/**
 * The leaf certificate the daemon at `url` presents, by a handshake that sends nothing.
 * Throws LeafError.
 */
export function fetchLeaf(url: RemoteUrl, timeoutMs: number): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    let secured = false;
    const name = serverName(url.host);
    const socket = tlsConnect({
      host: url.host,
      port: url.port,
      rejectUnauthorized: false,
      ALPNProtocols: ["h2"],
      minVersion: "TLSv1.2",
      ...(name === undefined ? {} : { servername: name }),
    });
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new LeafError("connect", `no answer within ${timeoutMs / 1000} s`, undefined));
    }, timeoutMs);
    socket.once("secureConnect", () => {
      secured = true;
      clearTimeout(timer);
      const peer: PeerCertificate = socket.getPeerCertificate(true);
      socket.destroy();
      if (!(peer.raw instanceof Buffer) || peer.raw.length === 0) {
        reject(new LeafError("tls", "the daemon presented no certificate", undefined));
        return;
      }
      resolve(peer.raw);
    });
    socket.once("error", (error: Error) => {
      clearTimeout(timer);
      socket.destroy();
      if (secured) {
        return;
      }
      const stage = isConnectFailure(error) && !isTlsCode(codeOf(error)) ? "connect" : "tls";
      reject(new LeafError(stage, error.message, error));
    });
  });
}

function isTlsCode(code: string | undefined): boolean {
  return code !== undefined && (code.startsWith("ERR_SSL") || code.startsWith("ERR_TLS"));
}

/**
 * The name the session uses for a pinned leaf: the URL's host if the leaf names it, else the
 * leaf's first DNS name, else its first IP address; undefined when it has no names.
 */
export function targetName(leaf: Leaf, host: string): string | undefined {
  const names = isIP(host) === 0 ? leaf.x509.checkHost(host) : leaf.x509.checkIP(host);
  if (names !== undefined) {
    return host;
  }
  const dns = leaf.names.find((n) => isIP(n) === 0);
  return dns ?? leaf.names[0];
}

/**
 * Throws RemoteTlsError (`handshake`) for a pinned certificate outside its dates. OpenSSL checks
 * a trusted root's dates even though the pin does not need them, and its failure would otherwise
 * be an opaque handshake error.
 */
export function checkDates(leaf: Leaf, origin: string, now: Date): void {
  if (now > leaf.notAfter) {
    throw new RemoteTlsError(origin, "handshake", {
      reason: `the daemon's certificate expired at ${utc(leaf.notAfter)} UTC`,
      hint:
        "The daemon's certificate has lapsed. A generated one is renewed when the daemon " +
        "starts: restart it on its machine. Renew a supplied one (`[remote] tls_cert`).",
    });
  }
  if (now < leaf.notBefore) {
    throw new RemoteTlsError(origin, "handshake", {
      reason:
        `the daemon's certificate is valid only from ${utc(leaf.notBefore)} UTC; ` +
        "check this machine's clock",
    });
  }
}

function utc(date: Date): string {
  return date.toISOString().slice(0, 16).replace("T", " ");
}

/** The error a pin check in a handshake reports, which the client maps to RemoteTlsError. */
export class PinMismatch extends Error {
  readonly presented: string;
  readonly expected: string;
  readonly code = "ERR_REEF_PIN_MISMATCH";
  constructor(presented: string, expected: string) {
    super(`the daemon presented ${presented}, not the pinned ${expected}`);
    this.presented = presented;
    this.expected = expected;
  }
}

/** A `checkServerIdentity` that accepts only a certificate with the pinned key. */
export function pinCheck(pin: string): (host: string, cert: PeerCertificate) => Error | undefined {
  return (_host, cert) => {
    try {
      const presented = fingerprintOf(cert.raw);
      return presented === pin ? undefined : new PinMismatch(presented, pin);
    } catch (error) {
      return error instanceof Error ? error : new Error(String(error));
    }
  };
}

/** OpenSSL's verify codes: the handshake refused the certificate it was given to trust. */
export const VERIFY_CODES = new Set([
  "DEPTH_ZERO_SELF_SIGNED_CERT",
  "SELF_SIGNED_CERT_IN_CHAIN",
  "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
  "UNABLE_TO_GET_ISSUER_CERT",
  "UNABLE_TO_GET_ISSUER_CERT_LOCALLY",
  "CERT_SIGNATURE_FAILURE",
  "CERT_UNTRUSTED",
  "CERT_REJECTED",
  "CERT_HAS_EXPIRED",
  "CERT_NOT_YET_VALID",
  "ERR_TLS_CERT_ALTNAME_INVALID",
  "INVALID_CA",
]);
