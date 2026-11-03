// reef-vcs: the TypeScript SDK for Reef (R2.07, ADR 0054).

export { Client, connect, incompatible, isUnserved, type ClientOptions } from "./client.js";
export {
  AlreadyExistsError,
  Code,
  ConfigError,
  ConflictError,
  DaemonNotRunningError,
  IncompatibleError,
  InternalError,
  InvalidArgumentError,
  IoError,
  isReefError,
  NotFoundError,
  PermissionDeniedError,
  ReefError,
  RemoteConnectError,
  RemoteMisdirectedError,
  RemoteTlsError,
  UnavailableError,
  UnsupportedError,
  type ErrorKind,
  type ReefErrorInit,
  type TlsFailure,
} from "./errors.js";
export { API_REVISION, COMMIT, VERSION } from "./gen/build.js";
export { RPCS } from "./gen/rpcs.js";
export type { CallOptions, ConnectOptions } from "./options.js";
export { BURST, REFRESH, Reporter, type ReporterOptions } from "./presence.js";
export type { Cadence, State, Subject } from "./presence-core.js";
export type { Access, Rpc } from "./rpc.js";
export { Stream } from "./stream.js";
export { WorkspaceHandle, type AttachOptions, type WorkspaceOptions } from "./workspace.js";
