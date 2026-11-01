// The daemon's errors as the app shows them: the message, and the
// `ErrorDetail`'s hint and `next` commands (ADR 0029 decision 14).
import { Code, ConnectError } from "@connectrpc/connect";
import { ErrorDetailSchema, ErrorKind } from "../gen/reef/api/v1/error_pb";

export interface ReefError {
  code: Code;
  /** `ErrorKind` as the CLI names it (`not_found`, `conflict`, ...); empty without a detail. */
  kind: string;
  reason: string;
  message: string;
  hint: string;
  next: string[];
  metadata: Record<string, string>;
}

export const WEB_SESSION_INVALID = "WEB_SESSION_INVALID";
/** The remote listener refused the page's token: revoked, expired or mistyped (R2.08). */
export const REMOTE_TOKEN_INVALID = "REMOTE_TOKEN_INVALID";
export const TOO_FAR_BEHIND = "TOO_FAR_BEHIND";
export const OPERATION_LOG_REWRITTEN = "OPERATION_LOG_REWRITTEN";

function kindName(kind: ErrorKind): string {
  // A kind newer than this build's schema has no name.
  const name = (ErrorKind as Record<number, string | undefined>)[kind];
  return kind === ErrorKind.UNSPECIFIED ? "" : (name?.toLowerCase() ?? "");
}

export function reefError(error: unknown): ReefError {
  const connect = ConnectError.from(error);
  const detail = connect.findDetails(ErrorDetailSchema)[0];
  return {
    code: connect.code,
    kind: detail ? kindName(detail.kind) : "",
    reason: detail?.reason ?? "",
    message: connect.rawMessage,
    hint: detail?.hint ?? "",
    next: detail?.next ?? [],
    metadata: { ...detail?.metadata },
  };
}

/**
 * The daemon refused the page's token: on loopback, the session ended (the
 * daemon restarted, or it idled out); over the remote listener, the token was
 * revoked, expired or mistyped (R2.08). Either way it is dead.
 */
export function isSessionInvalid(error: ReefError): boolean {
  return (
    error.code === Code.PermissionDenied &&
    (error.reason === WEB_SESSION_INVALID || error.reason === REMOTE_TOKEN_INVALID)
  );
}
