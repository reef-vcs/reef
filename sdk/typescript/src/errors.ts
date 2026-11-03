// The daemon's errors as classes by kind (ADR 0029, ADR 0054).
//
// Every failure is a ReefError of one class per kind. An error the daemon sent carries its
// ErrorDetail: `reason` (such as `WORKSPACE_NOT_FOUND`), `hint` (one sentence on how to fix it),
// `next` (a `reef` command line that does, as an argv array) and `metadata`. Failures found by the
// SDK itself (no daemon on the socket, a refused certificate, a mismatched daemon) are subclasses
// with their own advice.

import { Code, ConnectError } from "@connectrpc/connect";

import { ErrorDetailSchema, ErrorKind as ProtoKind } from "./gen/reef/api/v1/error_pb.js";
import type { ErrorDetail } from "./gen/reef/api/v1/error_pb.js";

export { Code };

/** What kind of failure an error is, in the CLI's words. */
export type ErrorKind =
  | "not_found"
  | "already_exists"
  | "invalid_argument"
  | "conflict"
  | "unavailable"
  | "permission_denied"
  | "io"
  | "internal"
  | "unsupported";

const BRAND = Symbol.for("reef-vcs.error");

/** The fields a ReefError is made from. */
export interface ReefErrorInit {
  kind?: ErrorKind | undefined;
  reason?: string | undefined;
  chain?: readonly string[] | undefined;
  hint?: string | undefined;
  next?: readonly string[] | undefined;
  metadata?: Readonly<Record<string, string>> | undefined;
  code?: Code | undefined;
  method?: string | undefined;
  cause?: unknown;
}

/** A failure of a call to the daemon, or to reach it. */
export class ReefError extends Error {
  /** The error's kind. */
  readonly kind: ErrorKind;
  /** The specific condition, such as `WORKSPACE_NOT_FOUND`; empty when none. */
  readonly reason: string;
  /** The error's message and each cause's, outermost first. */
  readonly chain: string[];
  /** How to fix it, in a sentence; empty when none. */
  readonly hint: string;
  /** A `reef` command line that fixes it, as argv; empty when none. */
  readonly next: string[];
  /** Values that go with `reason`, such as `{ workspace: "..." }`. */
  readonly metadata: Record<string, string>;
  /** The gRPC status code, or undefined for a failure found before a call. */
  readonly code: Code | undefined;
  /** The RPC's full path, or undefined. */
  readonly method: string | undefined;

  /** The kind an instance of this class has unless told otherwise. */
  static readonly defaultKind: ErrorKind = "internal";

  constructor(message: string, init: ReefErrorInit = {}) {
    super(message, init.cause === undefined ? undefined : { cause: init.cause });
    const defaultKind = new.target.defaultKind;
    this.kind = init.kind ?? defaultKind;
    this.reason = init.reason ?? "";
    this.chain = init.chain && init.chain.length > 0 ? [...init.chain] : message ? [message] : [];
    this.hint = init.hint ?? "";
    this.next = [...(init.next ?? [])];
    this.metadata = { ...init.metadata };
    this.code = init.code;
    this.method = init.method;
    Object.defineProperty(this, BRAND, { value: true });
    Object.defineProperty(this, "name", { value: new.target.name, enumerable: false });
  }
}

/** Whether `value` is a ReefError, from this copy of the package or another one. */
export function isReefError(value: unknown): value is ReefError {
  return (
    typeof value === "object" &&
    value !== null &&
    (value as Record<symbol, unknown>)[BRAND] === true
  );
}

/** The thing named does not exist. */
export class NotFoundError extends ReefError {
  static override readonly defaultKind: ErrorKind = "not_found";
}

/** The thing to create exists already. */
export class AlreadyExistsError extends ReefError {
  static override readonly defaultKind: ErrorKind = "already_exists";
}

/** The request is wrong as sent. */
export class InvalidArgumentError extends ReefError {
  static override readonly defaultKind: ErrorKind = "invalid_argument";
}

/** The request conflicts with the current state (gRPC `FAILED_PRECONDITION`). */
export class ConflictError extends ReefError {
  static override readonly defaultKind: ErrorKind = "conflict";
}

/** The daemon, or something it needs, is not available now; retrying may succeed. */
export class UnavailableError extends ReefError {
  static override readonly defaultKind: ErrorKind = "unavailable";
}

/** The caller may not do this. */
export class PermissionDeniedError extends ReefError {
  static override readonly defaultKind: ErrorKind = "permission_denied";
}

/** A filesystem failure on the daemon's machine. */
export class IoError extends ReefError {
  static override readonly defaultKind: ErrorKind = "io";
}

/** A bug, or a failure the daemon did not classify. */
export class InternalError extends ReefError {
  static override readonly defaultKind: ErrorKind = "internal";
}

/** The daemon cannot do this here (gRPC `UNIMPLEMENTED`). */
export class UnsupportedError extends ReefError {
  static override readonly defaultKind: ErrorKind = "unsupported";
}

/** Nothing answered on the daemon's socket. The SDK never starts a daemon. */
export class DaemonNotRunningError extends UnavailableError {
  /** The socket nothing answered on. */
  readonly socket: string;

  constructor(socket: string, cause = "", error?: unknown) {
    const message = `connect to the daemon at ${socket}`;
    super(message, {
      chain: cause ? [message, cause] : [message],
      hint: "Start the daemon: `reef daemon start`, or run any `reef` command, which starts it.",
      next: ["reef", "daemon", "start"],
      cause: error,
    });
    this.socket = socket;
  }
}

/** Nothing answered at a remote daemon's URL. */
export class RemoteConnectError extends UnavailableError {
  /** The remote's origin. */
  readonly url: string;

  constructor(url: string, cause = "", error?: unknown) {
    const message = `connect to the daemon at ${url}`;
    super(message, {
      chain: cause ? [message, cause] : [message],
      hint:
        "Nothing answered there: is the daemon running with its remote listener " +
        "(`reef daemon status` on that machine says `remote: listening`), and is the port " +
        "open to this machine?",
      cause: error,
    });
    this.url = url;
  }
}

/** Why a remote daemon's certificate was refused. */
export type TlsFailure = "pin_mismatch" | "untrusted" | "handshake";

/** A remote daemon's certificate was refused, before anything was sent. */
export class RemoteTlsError extends PermissionDeniedError {
  /** The remote's origin. */
  readonly url: string;
  /** `pin_mismatch`, `untrusted` or `handshake`. */
  readonly failure: TlsFailure;
  /** The fingerprint the daemon presented, when it was read. */
  readonly presented: string;
  /** The pinned fingerprint, for `pin_mismatch`. */
  readonly expected: string;

  constructor(
    url: string,
    failure: TlsFailure,
    options: {
      presented?: string;
      expected?: string;
      reason?: string;
      hint?: string;
      cause?: unknown;
    } = {},
  ) {
    const presented = options.presented ?? "";
    const expected = options.expected ?? "";
    const reason = options.reason ?? "";
    let cause: string;
    let hint: string;
    if (failure === "pin_mismatch") {
      cause = `the daemon presented ${presented}, not the pinned ${expected}`;
      hint =
        `The daemon presented ${presented}, not the pinned key. If \`reef remote show\` on ` +
        `the daemon's machine prints ${presented} (its key changes after a restore), add ` +
        `the remote again with --fingerprint ${presented}; otherwise something else ` +
        "answered at that address.";
    } else if (failure === "untrusted") {
      const shown = presented || "an unread certificate";
      cause = `the daemon's certificate (${shown}) is not trusted: ${reason}`;
      hint =
        "The daemon's certificate is not one this machine trusts. If `reef remote show` " +
        `on the daemon's machine prints ${shown}, pin it: pass fingerprint: "${shown}", set ` +
        "REEF_REMOTE_FINGERPRINT, or add the remote with --fingerprint.";
    } else {
      cause = `the TLS handshake failed: ${reason}`;
      hint = "The TLS handshake failed: check that the URL names the daemon's remote listener.";
    }
    const message = `refused the daemon at ${url}`;
    super(message, { chain: [message, cause], hint: options.hint ?? hint, cause: options.cause });
    this.url = url;
    this.failure = failure;
    this.presented = presented;
    this.expected = expected;
  }
}

/** A remote daemon answered that it does not serve the URL's host name (HTTP 421). */
export class RemoteMisdirectedError extends NotFoundError {
  /** The remote's origin. */
  readonly url: string;
  /** The host name it does not serve. */
  readonly host: string;

  constructor(url: string, host: string, cause?: unknown) {
    super(`the daemon at ${url} does not serve the name ${host}`, {
      hint:
        `Add \`${host}\` to \`[remote] names\` in the daemon's config.toml and restart it, or ` +
        "connect by a name or IP address it serves (`reef remote show` on that machine " +
        "lists them).",
      cause,
    });
    this.url = url;
    this.host = host;
  }
}

/** The socket, the configuration file or a remote profile could not be settled. */
export class ConfigError extends InvalidArgumentError {}

/** The daemon is another version and cannot serve the call. */
export class IncompatibleError extends UnsupportedError {
  /** The daemon's version; `unknown` when it would not say. */
  readonly daemonVersion: string;
  /** `v0` (reef 0.1) or `v1`. */
  readonly daemonApi: "v0" | "v1";
  /** The daemon's API revision, when known. */
  readonly daemonRevision: number | undefined;
  /** Its active workspaces. */
  readonly activeWorkspaces: number;

  constructor(
    target: string,
    method: string,
    options: {
      daemonVersion: string;
      daemonApi: "v0" | "v1";
      activeWorkspaces: number;
      daemonRevision?: number | undefined;
      sdkVersion: string;
      detail?: string;
      cause?: unknown;
    },
  ) {
    const { daemonVersion, daemonApi, sdkVersion } = options;
    const message =
      options.detail ??
      `the daemon at ${target} is reef ${daemonVersion} (API ${daemonApi}) and cannot ` +
        `serve ${method} for reef-vcs ${sdkVersion} (API v1)`;
    super(message, {
      reason: "API_VERSION_MISMATCH",
      hint:
        `The daemon is reef ${daemonVersion} (API ${daemonApi}); this SDK is reef-vcs ` +
        `${sdkVersion}. Restart the daemon with the matching reef (\`reef daemon stop\`, ` +
        `then any reef command), or install reef-vcs@${daemonVersion}.`,
      next: ["reef", "daemon", "stop"],
      metadata: { daemon_version: daemonVersion, daemon_api: daemonApi },
      method,
      cause: options.cause,
    });
    this.daemonVersion = daemonVersion;
    this.daemonApi = daemonApi;
    this.daemonRevision = options.daemonRevision;
    this.activeWorkspaces = options.activeWorkspaces;
  }
}

const CLASSES: Record<ErrorKind, new (message: string, init?: ReefErrorInit) => ReefError> = {
  not_found: NotFoundError,
  already_exists: AlreadyExistsError,
  invalid_argument: InvalidArgumentError,
  conflict: ConflictError,
  unavailable: UnavailableError,
  permission_denied: PermissionDeniedError,
  io: IoError,
  internal: InternalError,
  unsupported: UnsupportedError,
};

const PROTO_KINDS: Partial<Record<ProtoKind, ErrorKind>> = {
  [ProtoKind.NOT_FOUND]: "not_found",
  [ProtoKind.ALREADY_EXISTS]: "already_exists",
  [ProtoKind.INVALID_ARGUMENT]: "invalid_argument",
  [ProtoKind.CONFLICT]: "conflict",
  [ProtoKind.UNAVAILABLE]: "unavailable",
  [ProtoKind.PERMISSION_DENIED]: "permission_denied",
  [ProtoKind.IO]: "io",
  [ProtoKind.INTERNAL]: "internal",
  [ProtoKind.UNSUPPORTED]: "unsupported",
};

const CODE_KINDS: Partial<Record<Code, ErrorKind>> = {
  [Code.NotFound]: "not_found",
  [Code.AlreadyExists]: "already_exists",
  [Code.InvalidArgument]: "invalid_argument",
  [Code.FailedPrecondition]: "conflict",
  [Code.Unavailable]: "unavailable",
  [Code.PermissionDenied]: "permission_denied",
  [Code.Unimplemented]: "unsupported",
};

/** The hint for a request over the daemon's 4 MiB limit. */
export const TOO_LARGE_HINT =
  "Edit large files in a workspace, or split the edits across requests.";

/** The kind for a status without a Reef detail: the inverse of the daemon's code table. */
export function kindOfCode(code: Code | undefined): ErrorKind {
  return (code === undefined ? undefined : CODE_KINDS[code]) ?? "internal";
}

/** The kind an `ErrorDetail.kind` names; unspecified and unknown values are internal. */
export function kindOfProto(kind: ProtoKind): ErrorKind {
  return PROTO_KINDS[kind] ?? "internal";
}

/** The class for a kind. */
export function classOf(kind: ErrorKind): new (message: string, init?: ReefErrorInit) => ReefError {
  return CLASSES[kind];
}

/** The `reef.api.v1.ErrorDetail` a failed call carries, if any. */
export function detailOf(error: ConnectError): ErrorDetail | undefined {
  return error.findDetails(ErrorDetailSchema)[0];
}

/** A ConnectError as the error of its kind, with the daemon's detail when it has one. */
export function fromConnectError(error: ConnectError, method?: string): ReefError {
  const detail = detailOf(error);
  const message = error.rawMessage || Code[error.code];
  if (detail === undefined) {
    const kind = kindOfCode(error.code);
    return new CLASSES[kind](message, { kind, code: error.code, method, cause: error });
  }
  const kind = kindOfProto(detail.kind);
  return new CLASSES[kind](message, {
    kind,
    reason: detail.reason,
    chain: detail.chain,
    hint: detail.hint,
    next: detail.next,
    metadata: detail.metadata,
    code: error.code,
    method,
    cause: error,
  });
}
