// How a client reaches the daemon: its socket, or a remote listener (R2.08), as Connect's gRPC
// transport over node:http2.
//
// The socket's session sends `localhost` as `:authority`: a percent-encoded socket path, which
// some clients send, is reset by the daemon's HTTP/2 server. A pinned remote's session trusts only
// the leaf read beforehand and checks the key again in every handshake, so nothing (no token) is
// written to a server without the pinned key. connect-node keeps a session's socket unref'ed
// while no call or stream is open, so a script that never calls close() still exits.

import { connect as netConnect } from "node:net";
import { connect as tlsConnect, getCACertificates, type ConnectionOptions } from "node:tls";

import type { Transport as ConnectTransport } from "@connectrpc/connect";
import { createGrpcTransport, Http2SessionManager } from "@connectrpc/connect-node";

import { RemoteConnectError, RemoteTlsError } from "./errors.js";
import { authority, labelOf, type RemoteTarget } from "./remote.js";
import {
  checkDates,
  fetchLeaf,
  type Leaf,
  LeafError,
  parseLeaf,
  pemOf,
  pinCheck,
  serverName,
  targetName,
} from "./tls.js";

/** The largest message a daemon decodes or sends: tonic's default, which the daemon keeps. */
export const MAX_MESSAGE_BYTES = 4 << 20;
/** The keepalive ping interval on a remote session, as the Rust client keeps idle streams up. */
export const KEEPALIVE_MS = 30_000;

/** One HTTP/2 session and the Connect transport over it. */
export interface Session {
  readonly manager: Http2SessionManager;
  readonly transport: ConnectTransport;
  /** The pinned leaf the session trusts, for a pinned remote. */
  readonly leaf: Leaf | undefined;
}

/** What a client is connected to, shared by a client and its `withOptions` copies. */
export interface Target {
  /** How errors name it: the socket path, or the remote's origin. */
  readonly label: string;
  /** The socket, for a local daemon. */
  readonly socket: string | undefined;
  /** The remote, for a remote daemon. */
  readonly remote: RemoteTarget | undefined;
}

function grpc(baseUrl: string, manager: Http2SessionManager): ConnectTransport {
  return createGrpcTransport({
    baseUrl,
    sessionManager: manager,
    readMaxBytes: MAX_MESSAGE_BYTES,
    writeMaxBytes: MAX_MESSAGE_BYTES,
  });
}

/** A session on the daemon's socket. */
export function localSession(socket: string, connectTimeoutMs: number): Session {
  const manager = new Http2SessionManager(
    "http://localhost",
    { connectTimeoutMs },
    { createConnection: () => netConnect(socket) },
  );
  return { manager, transport: grpc("http://localhost", manager), leaf: undefined };
}

/** Node's bundled roots, NODE_EXTRA_CA_CERTS and the OS's store, without duplicates. */
export function defaultRoots(): string[] {
  const seen = new Set<string>();
  for (const store of ["default", "system"] as const) {
    try {
      for (const pem of getCACertificates(store)) {
        seen.add(pem.trim() + "\n");
      }
    } catch {
      // A store this platform does not have.
    }
  }
  return [...seen];
}

/**
 * A pinned remote's session: the leaf as the only root, the pin checked in every handshake, and
 * the URL's host replaced by a name the certificate lists when it lists another (ADR 0054
 * decision 6), with the connection still made to the URL's host.
 */
export function pinnedSession(target: RemoteTarget, leaf: Leaf, connectTimeoutMs: number): Session {
  const url = target.url;
  const pin = target.fingerprint ?? leaf.fingerprint;
  const name = targetName(leaf, url.host) ?? url.host;
  const tlsOptions: ConnectionOptions = {
    // Explicitly: Node's default follows NODE_TLS_REJECT_UNAUTHORIZED, and with it off a failed
    // chain check is ignored and checkServerIdentity is never called, so the pin would not run.
    rejectUnauthorized: true,
    ca: [pemOf(leaf.der)],
    checkServerIdentity: pinCheck(pin),
    ALPNProtocols: ["h2"],
    minVersion: "TLSv1.2",
  };
  const sni = serverName(name);
  if (sni !== undefined) {
    tlsOptions.servername = sni;
  }
  const baseUrl = `https://${authority(name, url.port)}`;
  const manager = new Http2SessionManager(
    baseUrl,
    { connectTimeoutMs, pingIntervalMs: KEEPALIVE_MS, pingIdleConnection: true },
    {
      ...tlsOptions,
      createConnection: () => tlsConnect({ ...tlsOptions, host: url.host, port: url.port }),
    },
  );
  return { manager, transport: grpc(baseUrl, manager), leaf };
}

/** A CA-verified remote's session: Node's checks, against `roots`, for the URL's host. */
export function caSession(
  target: RemoteTarget,
  roots: string[] | undefined,
  connectTimeoutMs: number,
): Session {
  const url = target.url;
  const tlsOptions: ConnectionOptions = {
    // Explicitly, whatever NODE_TLS_REJECT_UNAUTHORIZED says.
    rejectUnauthorized: true,
    ca: roots ?? defaultRoots(),
    ALPNProtocols: ["h2"],
    minVersion: "TLSv1.2",
  };
  const sni = serverName(url.host);
  if (sni !== undefined) {
    tlsOptions.servername = sni;
  }
  const manager = new Http2SessionManager(
    url.origin,
    { connectTimeoutMs, pingIntervalMs: KEEPALIVE_MS, pingIdleConnection: true },
    tlsOptions,
  );
  return { manager, transport: grpc(url.origin, manager), leaf: undefined };
}

/**
 * The leaf a pinned remote presents, checked against the pin and its dates. Throws
 * RemoteConnectError when nothing answered and RemoteTlsError when the handshake failed or the
 * key is not the pinned one.
 */
export async function readPinnedLeaf(target: RemoteTarget, timeoutMs: number): Promise<Leaf> {
  const origin = target.url.origin;
  let der: Buffer;
  try {
    der = await fetchLeaf(target.url, timeoutMs);
  } catch (error) {
    if (error instanceof LeafError && error.stage === "tls") {
      throw new RemoteTlsError(origin, "handshake", { reason: error.message, cause: error });
    }
    throw new RemoteConnectError(origin, error instanceof Error ? error.message : "", error);
  }
  let leaf: Leaf;
  try {
    leaf = parseLeaf(der);
  } catch (error) {
    throw new RemoteTlsError(origin, "handshake", {
      reason: `its certificate: ${error instanceof Error ? error.message : String(error)}`,
      cause: error,
    });
  }
  const pin = target.fingerprint ?? "";
  if (leaf.fingerprint !== pin) {
    throw new RemoteTlsError(origin, "pin_mismatch", {
      presented: leaf.fingerprint,
      expected: pin,
    });
  }
  checkDates(leaf, origin, new Date());
  return leaf;
}

/** The fingerprint a remote presents, for an error; a description when it cannot be read. */
export async function presentedBy(target: RemoteTarget, timeoutMs: number): Promise<string> {
  try {
    return parseLeaf(await fetchLeaf(target.url, timeoutMs)).fingerprint;
  } catch {
    return "";
  }
}

/** A client's connection: the target and its current session, which a re-pin replaces. */
export class Connection {
  readonly target: Target;
  readonly connectTimeoutMs: number;
  readonly #roots: string[] | undefined;
  #session: Session;
  #repinning: Promise<boolean> | undefined;
  #closed = false;
  /** The daemon's status, cached by `connect()` and `daemonStatus()`. */
  status: unknown;

  private constructor(
    target: Target,
    session: Session,
    connectTimeoutMs: number,
    roots: string[] | undefined,
  ) {
    this.target = target;
    this.#session = session;
    this.connectTimeoutMs = connectTimeoutMs;
    this.#roots = roots;
  }

  /** A connection to the daemon's socket. */
  static local(socket: string, connectTimeoutMs: number): Connection {
    return new Connection(
      { label: socket, socket, remote: undefined },
      localSession(socket, connectTimeoutMs),
      connectTimeoutMs,
      undefined,
    );
  }

  /** A connection to a remote daemon: pinned when the target has a fingerprint. */
  static async remote(
    target: RemoteTarget,
    connectTimeoutMs: number,
    roots: string[] | undefined,
  ): Promise<Connection> {
    const shared: Target = { label: target.url.origin, socket: undefined, remote: target };
    if (target.fingerprint) {
      const leaf = await readPinnedLeaf(target, connectTimeoutMs);
      return new Connection(
        shared,
        pinnedSession(target, leaf, connectTimeoutMs),
        connectTimeoutMs,
        roots,
      );
    }
    return new Connection(
      shared,
      caSession(target, roots, connectTimeoutMs),
      connectTimeoutMs,
      roots,
    );
  }

  /** The current session. */
  get session(): Session {
    return this.#session;
  }

  /** Whether `close()` was called. */
  get closed(): boolean {
    return this.#closed;
  }

  /** How errors name a remote: its profile, else its origin. */
  get remoteLabel(): string | undefined {
    return this.target.remote === undefined ? undefined : labelOf(this.target.remote);
  }

  /**
   * After a pinned session's handshake refused the leaf it trusts: whether the daemon now presents
   * a new certificate for the pinned key (a renewal), in which case the session is replaced. Throws
   * RemoteTlsError when it presents another key (`pin_mismatch`), a certificate outside its dates,
   * or a handshake that fails: a re-pin cannot fix those. False when nothing answered. One re-pin
   * runs at a time; a call that saw an older session than the current one just retries.
   */
  async repin(seen: Session): Promise<boolean> {
    const remote = this.target.remote;
    if (remote === undefined || seen.leaf === undefined || !remote.fingerprint || this.#closed) {
      return false;
    }
    if (this.#session !== seen) {
      return true;
    }
    this.#repinning ??= (async () => {
      try {
        let leaf: Leaf;
        try {
          leaf = await readPinnedLeaf(remote, this.connectTimeoutMs);
        } catch (error) {
          if (error instanceof RemoteTlsError) {
            throw error;
          }
          return false;
        }
        // close() may have run while the leaf was read: install nothing it would not abort.
        if (this.#closed || leaf.der.equals(seen.leaf?.der ?? Buffer.alloc(0))) {
          return false;
        }
        this.#session = pinnedSession(remote, leaf, this.connectTimeoutMs);
        seen.manager.abort();
        return true;
      } finally {
        this.#repinning = undefined;
      }
    })();
    return this.#repinning;
  }

  /** Closes the session, for every client sharing it. */
  close(): void {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.#session.manager.abort();
  }

  /** The roots a CA-verified remote trusts, when given. */
  get roots(): string[] | undefined {
    return this.#roots;
  }
}
