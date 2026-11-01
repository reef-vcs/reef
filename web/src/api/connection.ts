// How the app reaches a daemon. The browser build talks gRPC-web to the
// listener that served it; a shell (R3.39, R3.40) or a store (R4.13)
// supplies its own `Connection` instead, so nothing else in the app assumes
// the API is same-origin.
import { createClient, type Client, type Interceptor, type Transport } from "@connectrpc/connect";
import { createGrpcWebTransport } from "@connectrpc/connect-web";
import { ChangeService } from "../gen/reef/api/v1/changes_pb";
import { DaemonService } from "../gen/reef/api/v1/daemon_pb";
import { DiffService } from "../gen/reef/api/v1/diff_pb";
import { HistoryService } from "../gen/reef/api/v1/history_pb";
import { OperationService } from "../gen/reef/api/v1/operations_pb";
import { PresenceService } from "../gen/reef/api/v1/presence_pb";
import { RepositoryService } from "../gen/reef/api/v1/repositories_pb";
import { StatusService } from "../gen/reef/api/v1/status_pb";

export interface Clients {
  daemon: Client<typeof DaemonService>;
  repositories: Client<typeof RepositoryService>;
  operations: Client<typeof OperationService>;
  presence: Client<typeof PresenceService>;
  changes: Client<typeof ChangeService>;
  status: Client<typeof StatusService>;
  diff: Client<typeof DiffService>;
  history: Client<typeof HistoryService>;
}

export interface Connection {
  transport: Transport;
  clients: Clients;
  /** Where the connection goes, for display. */
  label: string;
}

/** Sends the session's token on every call. */
export function bearer(token: string): Interceptor {
  return (next) => (request) => {
    request.header.set("authorization", `Bearer ${token}`);
    return next(request);
  };
}

export function connectionOver(transport: Transport, label: string): Connection {
  return {
    transport,
    label,
    clients: {
      daemon: createClient(DaemonService, transport),
      repositories: createClient(RepositoryService, transport),
      operations: createClient(OperationService, transport),
      presence: createClient(PresenceService, transport),
      changes: createClient(ChangeService, transport),
      status: createClient(StatusService, transport),
      diff: createClient(DiffService, transport),
      history: createClient(HistoryService, transport),
    },
  };
}

/** gRPC-web, binary only, to `baseUrl` (this page's origin by default). */
export function browserConnection(token: string, baseUrl: string = location.origin): Connection {
  const transport = createGrpcWebTransport({
    baseUrl,
    useBinaryFormat: true,
    interceptors: [bearer(token)],
  });
  return connectionOver(transport, baseUrl);
}
