// The client (R2.07, ADR 0054): one typed method per RPC, generated in src/gen/methods.ts, over
// the call primitives here; `connect()`, which settles what it reaches exactly as the CLI does.

import {
  clone,
  create,
  type DescMessage,
  type DescMethodServerStreaming,
  type DescMethodUnary,
  type MessageInitShape,
  type MessageShape,
} from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { createAsyncIterable } from "@connectrpc/connect/protocol";

import {
  DaemonNotRunningError,
  detailOf,
  fromConnectError,
  IncompatibleError,
  InternalError,
  InvalidArgumentError,
  isReefError,
  ReefError,
  RemoteConnectError,
  RemoteMisdirectedError,
  RemoteTlsError,
  TOO_LARGE_HINT,
} from "./errors.js";
import { API_REVISION, VERSION } from "./gen/build.js";
import { Methods } from "./gen/methods.js";
import { type GetDaemonStatusResponse, DaemonService } from "./gen/reef/api/v1/daemon_pb.js";
import { Daemon as V0Daemon, StatusRequestSchema } from "./gen/reef/daemon/v0/daemon_pb.js";
import type { ReadFileResponse } from "./gen/reef/api/v1/files_pb.js";
import { ReadFileRequestSchema } from "./gen/reef/api/v1/files_pb.js";
import { AGENT_METADATA, agentFrom, checkAgent } from "./identity.js";
import type { CallOptions, ConnectOptions } from "./options.js";
import { settle, settleStateDir } from "./paths.js";
import { choose, REMOTE_ENV } from "./remote.js";
import type { Rpc } from "./rpc.js";
import { Stream } from "./stream.js";
import { codeOf, isConnectFailure, PinMismatch, VERIFY_CODES } from "./tls.js";
import { Connection, presentedBy, type Session, type Target } from "./transport.js";
import type { AttachOptions, WorkspaceHandle, WorkspaceOptions } from "./workspace.js";
import { attachWorkspace, openWorkspace } from "./workspace.js";

const UNSERVED_REASONS = new Set(["UNKNOWN_METHOD", "NOT_IMPLEMENTED"]);
const PROBE_TIMEOUT_MS = 5_000;
const DEFAULT_CONNECT_TIMEOUT_MS = 5_000;
// The listener's HTTP 421, as connect-node reports it.
const MISDIRECTED = /\bHTTP 421\b/;
// connect-node's own timeout for a connection, on a reconnect after connect().
const CONNECT_TIMED_OUT = /^connection establishment timed out$/;
// Connect's refusal of a request over writeMaxBytes, before anything is sent.
const TOO_LARGE = /larger than configured writeMaxBytes/;

/** Every cause in an error's chain, the error first. */
function causes(error: unknown): unknown[] {
  const out: unknown[] = [];
  let at: unknown = error;
  while (at !== undefined && at !== null && out.length < 8 && !out.includes(at)) {
    out.push(at);
    at = (at as { cause?: unknown }).cause;
  }
  return out;
}

/** The first Node error code in an error's causes. */
function nodeCode(error: unknown): string | undefined {
  for (const cause of causes(error)) {
    const code = codeOf(cause);
    if (code !== undefined) {
      return code;
    }
  }
  return undefined;
}

function pinMismatchOf(error: unknown): PinMismatch | undefined {
  return causes(error).find((c): c is PinMismatch => c instanceof PinMismatch);
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Whether a failure was a pinned session's handshake refusing the leaf it trusts. */
function refusedHandshake(session: Session, error: unknown): boolean {
  if (session.leaf === undefined) {
    return false;
  }
  const code = nodeCode(error);
  return code !== undefined && VERIFY_CODES.has(code);
}

/** The client's options a copy may change. */
export interface ClientOptions {
  /** The agent named on every call; `undefined` sends none. */
  agent?: string | undefined;
  /** Every unary call's default deadline in milliseconds. */
  timeoutMs?: number | undefined;
}

/**
 * A connection to a Reef daemon, with one method per RPC. Made by {@link connect}. Closing it
 * (`close()`, or leaving `await using`) closes the connection for every copy `withOptions` made.
 */
export class Client extends Methods implements AsyncDisposable {
  readonly #connection: Connection;
  readonly #agent: string | undefined;
  readonly #timeoutMs: number | undefined;

  /** @internal */
  constructor(connection: Connection, agent: string | undefined, timeoutMs: number | undefined) {
    super();
    this.#connection = connection;
    this.#agent = agent;
    this.#timeoutMs = timeoutMs;
  }

  /** The daemon's socket path, or the remote daemon's origin. */
  get target(): string {
    return this.#connection.target.label;
  }

  /** The name sent as `reef-agent` on every call, if any. */
  get agent(): string | undefined {
    return this.#agent;
  }

  /** Whether this is a remote daemon. */
  get remote(): boolean {
    return this.#connection.target.remote !== undefined;
  }

  /** A client sharing this one's connection, with another agent or default deadline. */
  withOptions(options: ClientOptions): Client {
    const agent = Object.hasOwn(options, "agent")
      ? options.agent === undefined
        ? undefined
        : checkAgent(options.agent)
      : this.#agent;
    const timeoutMs = Object.hasOwn(options, "timeoutMs") ? options.timeoutMs : this.#timeoutMs;
    return new Client(this.#connection, agent, timeoutMs);
  }

  /** Closes the connection, for every client sharing it. */
  close(): Promise<void> {
    this.#connection.close();
    return Promise.resolve();
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }

  #headers(): Record<string, string> {
    const headers: Record<string, string> = {};
    if (this.#agent !== undefined) {
      headers[AGENT_METADATA] = this.#agent;
    }
    const remote = this.#connection.target.remote;
    if (remote !== undefined) {
      headers["authorization"] = `Bearer ${remote.token}`;
    }
    return headers;
  }

  #checkOpen(rpc: Rpc): void {
    if (this.#connection.closed) {
      throw new InternalError(`the client is closed; connect again to call ${rpc.name}`, {
        method: rpc.path,
      });
    }
  }

  /** @internal */
  protected override async unaryCall<I extends DescMessage, O extends DescMessage>(
    rpc: Rpc<I, O>,
    request: MessageInitShape<I> | undefined,
    options: CallOptions | undefined,
  ): Promise<MessageShape<O>> {
    this.#checkOpen(rpc);
    const method = rpc.method as DescMethodUnary<I, O>;
    const timeoutMs = options?.timeoutMs ?? this.#timeoutMs;
    const input = request ?? ({} as MessageInitShape<I>);
    let retried = false;
    for (;;) {
      const session = this.#connection.session;
      try {
        const response = await session.transport.unary(
          method,
          options?.signal,
          timeoutMs,
          this.#headers(),
          input,
        );
        return response.message;
      } catch (error) {
        // A refused handshake sent nothing, so any call may be retried once on a new session.
        if (!retried && refusedHandshake(session, error)) {
          retried = true;
          if (await this.#connection.repin(session)) {
            continue;
          }
        }
        throw await this.#error(rpc, error);
      }
    }
  }

  /** @internal */
  protected override streamCall<I extends DescMessage, O extends DescMessage>(
    rpc: Rpc<I, O>,
    request: MessageInitShape<I> | undefined,
    options: CallOptions | undefined,
  ): Stream<MessageShape<O>> {
    this.#checkOpen(rpc);
    const method = rpc.method as DescMethodServerStreaming<I, O>;
    const input = request ?? ({} as MessageInitShape<I>);
    const open = async (signal: AbortSignal): Promise<AsyncIterable<MessageShape<O>>> => {
      let retried = false;
      for (;;) {
        const session = this.#connection.session;
        try {
          const response = await session.transport.stream(
            method,
            signal,
            undefined,
            this.#headers(),
            createAsyncIterable([input]),
          );
          return response.message;
        } catch (error) {
          if (!retried && !signal.aborted && refusedHandshake(session, error)) {
            retried = true;
            if (await this.#connection.repin(session)) {
              continue;
            }
          }
          throw error;
        }
      }
    };
    const mapError = async (error: unknown): Promise<unknown> => {
      const session = this.#connection.session;
      if (refusedHandshake(session, error)) {
        try {
          await this.#connection.repin(session);
        } catch (refused) {
          return refused;
        }
      }
      return this.#error(rpc, error);
    };
    return new Stream(open, mapError, options?.signal);
  }

  /** @internal */
  protected override async *pagesCall<I extends DescMessage, O extends DescMessage>(
    rpc: Rpc<I, O>,
    request: MessageInitShape<I> | undefined,
    options: CallOptions | undefined,
  ): AsyncGenerator<MessageShape<O>> {
    const schema = rpc.method.input;
    const pageRequest = clone(schema, create(schema, request));
    for (;;) {
      const page = await this.unaryCall(rpc, pageRequest, options);
      yield page;
      const token = (page as { nextPageToken?: string }).nextPageToken ?? "";
      if (!token) {
        return;
      }
      (pageRequest as { pageToken?: string }).pageToken = token;
    }
  }

  /** @internal */
  protected override async *itemsCall<I extends DescMessage, O extends DescMessage, T>(
    rpc: Rpc<I, O>,
    request: MessageInitShape<I> | undefined,
    options: CallOptions | undefined,
    field: string,
  ): AsyncGenerator<T> {
    for await (const page of this.pagesCall(rpc, request, options)) {
      yield* (page as unknown as Record<string, T[]>)[field] ?? [];
    }
  }

  /** A failed call as a ReefError. */
  async #error(rpc: Rpc, error: unknown): Promise<ReefError> {
    const remote = this.#connection.target.remote;
    let presented = "";
    if (remote !== undefined && !isReefError(error)) {
      const code = nodeCode(error);
      if (code !== undefined && VERIFY_CODES.has(code) && pinMismatchOf(error) === undefined) {
        presented = await presentedBy(remote, this.#connection.connectTimeoutMs);
      }
    }
    const mapped = classify(error, this.#connection.target, rpc.path, presented);
    if (mapped === undefined && error instanceof ConnectError) {
      return this.#probe(rpc.path, error);
    }
    return mapped ?? new InternalError(messageOf(error), { method: rpc.path, cause: error });
  }

  /** What a daemon that could not serve `method` is, from its two status calls. */
  async #probe(method: string, error: ConnectError): Promise<ReefError> {
    const session = this.#connection.session;
    const headers = this.#headers();
    try {
      const v1 = await session.transport.unary(
        DaemonService.method.getDaemonStatus,
        undefined,
        PROBE_TIMEOUT_MS,
        headers,
        { skipBackends: true },
      );
      return incompatible(this.target, method, error, v1.message, undefined);
    } catch {
      // Not a v1 daemon, or not answering.
    }
    try {
      const v0 = await session.transport.unary(
        V0Daemon.method.status,
        undefined,
        PROBE_TIMEOUT_MS,
        headers,
        create(StatusRequestSchema),
      );
      return incompatible(this.target, method, error, undefined, v0.message);
    } catch {
      return incompatible(this.target, method, error, undefined, undefined);
    }
  }

  /** `GetDaemonStatus` without the backend probe, cached for the connection's life. */
  async daemonStatus(options: { refresh?: boolean } = {}): Promise<GetDaemonStatusResponse> {
    const cached = this.#connection.status as GetDaemonStatusResponse | undefined;
    if (cached !== undefined && options.refresh !== true) {
      return cached;
    }
    const status = await this.getDaemonStatus({ skipBackends: true });
    this.#connection.status = status;
    return status;
  }

  /** Throws IncompatibleError when the daemon's API revision is below `revision`. */
  async requireRevision(revision: number): Promise<void> {
    const status = await this.daemonStatus();
    if (status.apiRevision >= revision) {
      return;
    }
    throw new IncompatibleError(this.target, `API revision ${revision}`, {
      daemonVersion: status.version || "unknown",
      daemonApi: "v1",
      daemonRevision: status.apiRevision,
      activeWorkspaces: status.activeWorkspaceCount,
      sdkVersion: VERSION,
      detail:
        `the daemon at ${this.target} is reef ${status.version} at API revision ` +
        `${status.apiRevision}; this needs revision ${revision}`,
    });
  }

  /**
   * A whole file: the first read at the request's revision, then the rest from the commit it
   * returned, so every chunk comes from one commit. `content` holds every byte; the other fields
   * are the first response's.
   */
  async readFileToEnd(
    request?: MessageInitShape<typeof ReadFileRequestSchema>,
    options?: CallOptions,
  ): Promise<ReadFileResponse> {
    const base = clone(ReadFileRequestSchema, create(ReadFileRequestSchema, request));
    base.limit = 0n;
    const start = base.offset;
    const whole = await this.readFile(base, options);
    const chunks: Uint8Array[] = [whole.content];
    let read = BigInt(whole.content.length);
    const next = clone(ReadFileRequestSchema, base);
    next.revision = whole.commitId;
    next.skipSnapshot = true;
    for (;;) {
      next.offset = start + read;
      if (next.offset >= whole.size) {
        break;
      }
      const page = await this.readFile(next, options);
      if (page.content.length === 0) {
        break;
      }
      chunks.push(page.content);
      read += BigInt(page.content.length);
    }
    if (chunks.length > 1) {
      whole.content = new Uint8Array(Buffer.concat(chunks));
    }
    return whole;
  }

  /**
   * A new workspace for a block of code, held as `reef ws run` holds one: its lease renewed and
   * one presence entry reported while it is held, then released with a final snapshot unless
   * `keep`. Dispose the handle (`await using`) to release it, or use {@link withWorkspace}.
   */
  workspace(options: WorkspaceOptions): Promise<WorkspaceHandle> {
    return openWorkspace(this, options);
  }

  /**
   * Runs `fn` with a new workspace, then releases it. A release failure is thrown only when `fn`
   * succeeded; otherwise it is a warning and `fn`'s error propagates.
   */
  async withWorkspace<R>(
    options: WorkspaceOptions,
    fn: (workspace: WorkspaceHandle) => Promise<R> | R,
  ): Promise<R> {
    return run(await this.workspace(options), fn);
  }

  /**
   * An existing workspace for a block of code, under `ws run --workspace`'s rule: active, with a
   * live lease, held by `owner` or by this user on this machine. Released at the end only with
   * `release: true`.
   */
  attach(workspace: string, options: AttachOptions = {}): Promise<WorkspaceHandle> {
    return attachWorkspace(this, workspace, options);
  }

  /** Runs `fn` with an existing workspace held; see {@link attach}. */
  async withAttached<R>(
    workspace: string,
    options: AttachOptions,
    fn: (workspace: WorkspaceHandle) => Promise<R> | R,
  ): Promise<R> {
    return run(await this.attach(workspace, options), fn);
  }

  /** @internal The connection, for the workspace block and the probe at connect. */
  get connection(): Connection {
    return this.#connection;
  }
}

async function run<R>(
  handle: WorkspaceHandle,
  fn: (workspace: WorkspaceHandle) => Promise<R> | R,
): Promise<R> {
  let result: R;
  try {
    result = await fn(handle);
  } catch (error) {
    await handle.finish(error);
    throw error;
  }
  await handle.finish(undefined);
  return result;
}

/**
 * A failed call as the ReefError it is, in order: a pin refused in a handshake, a TLS
 * verification or handshake failure, a connection that failed (Connect reports these as
 * `Internal` with the Node error as `cause`), the listener's HTTP 421, a request over the 4 MiB
 * limit, then the daemon's detail, then the code. Undefined for an error that may say the daemon
 * cannot serve the method, which the caller probes. `presented` is the fingerprint a CA-verified
 * remote presented, for an untrusted certificate.
 */
export function classify(
  error: unknown,
  target: Target,
  method: string,
  presented = "",
): ReefError | undefined {
  if (isReefError(error)) {
    return error;
  }
  const remote = target.remote;
  const pin = pinMismatchOf(error);
  if (pin !== undefined && remote !== undefined) {
    return new RemoteTlsError(remote.url.origin, "pin_mismatch", {
      presented: pin.presented,
      expected: pin.expected,
      cause: error,
    });
  }
  const code = nodeCode(error);
  if (code !== undefined && remote !== undefined) {
    if (VERIFY_CODES.has(code)) {
      return new RemoteTlsError(remote.url.origin, "untrusted", {
        presented,
        reason: code,
        cause: error,
      });
    }
    if (code.startsWith("ERR_SSL") || code.startsWith("ERR_TLS")) {
      return new RemoteTlsError(remote.url.origin, "handshake", {
        reason: messageOf(causes(error).at(-1)),
        cause: error,
      });
    }
  }
  if (code !== undefined && isConnectFailure({ code })) {
    const cause = `${code}: ${messageOf(causes(error).at(-1))}`;
    return remote === undefined
      ? new DaemonNotRunningError(target.label, cause, error)
      : new RemoteConnectError(remote.url.origin, cause, error);
  }
  if (!(error instanceof ConnectError)) {
    return new InternalError(messageOf(error), { method, cause: error });
  }
  if (
    error.code === Code.Unavailable &&
    CONNECT_TIMED_OUT.test(error.rawMessage) &&
    detailOf(error) === undefined
  ) {
    return remote === undefined
      ? new DaemonNotRunningError(target.label, error.rawMessage, error)
      : new RemoteConnectError(remote.url.origin, error.rawMessage, error);
  }
  if (
    remote !== undefined &&
    error.code === Code.Unknown &&
    MISDIRECTED.test(error.rawMessage) &&
    detailOf(error) === undefined
  ) {
    return new RemoteMisdirectedError(remote.url.origin, remote.url.host, error);
  }
  if (error.code === Code.ResourceExhausted && TOO_LARGE.test(error.rawMessage)) {
    return new InvalidArgumentError(error.rawMessage, {
      hint: TOO_LARGE_HINT,
      code: error.code,
      method,
      cause: error,
    });
  }
  if (isUnserved(error)) {
    return undefined;
  }
  return fromConnectError(error, method);
}

/**
 * Whether an error may say the daemon cannot serve the method: a bare `Unimplemented`, or one
 * whose reason is `UNKNOWN_METHOD` or `NOT_IMPLEMENTED`.
 */
export function isUnserved(error: ConnectError): boolean {
  if (error.code !== Code.Unimplemented) {
    return false;
  }
  const detail = detailOf(error);
  return detail === undefined || UNSERVED_REASONS.has(detail.reason);
}

/**
 * What a daemon that could not serve `method` is, from its answers to the two status calls
 * (`v1`: a `GetDaemonStatusResponse`; `v0`: 0.1's `StatusResponse`).
 */
export function incompatible(
  target: string,
  method: string,
  error: ConnectError,
  v1: { version: string; apiRevision: number; activeWorkspaceCount: number } | undefined,
  v0: { version: string; activeWorkspaces: number } | undefined,
): ReefError {
  const original = fromConnectError(error, method);
  if (v1 !== undefined) {
    const detail = detailOf(error);
    if (v1.version === VERSION && detail?.reason === "NOT_IMPLEMENTED") {
      return original;
    }
    return new IncompatibleError(target, method, {
      daemonVersion: v1.version || "unknown",
      daemonApi: "v1",
      daemonRevision: v1.apiRevision,
      activeWorkspaces: v1.activeWorkspaceCount,
      sdkVersion: VERSION,
      cause: error,
    });
  }
  if (v0 !== undefined) {
    return new IncompatibleError(target, method, {
      daemonVersion: v0.version || "unknown",
      daemonApi: "v0",
      activeWorkspaces: v0.activeWorkspaces,
      sdkVersion: VERSION,
      cause: error,
    });
  }
  return original;
}

/**
 * Connects to a Reef daemon: the socket settled as the CLI settles it, or a remote daemon by URL
 * or profile (with the key pinned before the token is sent, or CA-verified). Calls
 * `GetDaemonStatus` before returning, so a daemon that is not running, or of another API, is
 * named here. Never starts a daemon.
 */
export async function connect(options: ConnectOptions = {}): Promise<Client> {
  const agent = agentFrom(options.agent);
  const connectTimeoutMs = options.connectTimeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS;
  if (options.socket !== undefined && options.remote !== undefined) {
    throw new TypeError("connect() takes a socket or a remote, not both");
  }
  let remote = options.remote;
  if (options.socket === undefined && remote === undefined) {
    remote = process.env[REMOTE_ENV]?.trim() || undefined;
  }
  let connection: Connection;
  if (remote !== undefined) {
    const stateDir = settleStateDir({ stateDir: options.stateDir });
    const target = choose(remote, {
      stateDir,
      token: options.token,
      fingerprint: options.fingerprint,
    });
    connection = await Connection.remote(target, connectTimeoutMs, options.rootCertificates);
  } else {
    const settled = settle({
      socket: options.socket,
      stateDir: options.stateDir,
      config: options.config,
    });
    connection = Connection.local(settled.socket, connectTimeoutMs);
  }
  const client = new Client(connection, agent, options.timeoutMs);
  try {
    connection.status = await client.getDaemonStatus(
      { skipBackends: true },
      { timeoutMs: connectTimeoutMs },
    );
  } catch (error) {
    connection.close();
    if (isReefError(error) && error.code === Code.DeadlineExceeded) {
      const cause = `no answer within ${connectTimeoutMs / 1000} s`;
      throw connection.target.remote === undefined
        ? new DaemonNotRunningError(connection.target.label, cause, error)
        : new RemoteConnectError(connection.target.label, cause, error);
    }
    throw error;
  }
  return client;
}

/** The API revision this SDK was generated at. */
export { API_REVISION };
