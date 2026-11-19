// How the app reaches a daemon. The browser build talks gRPC-web to the
// listener that served it; a shell (R3.39, R3.40) or a store (R4.13)
// supplies its own `Connection` instead, so nothing else in the app assumes
// the API is same-origin.
import type { DescService } from "@bufbuild/protobuf";
import { createClient, type Client, type Interceptor, type Transport } from "@connectrpc/connect";
import { createGrpcWebTransport } from "@connectrpc/connect-web";

export interface Connection {
  transport: Transport;
  /** Where the connection goes, for display. */
  label: string;
}

const clients = new WeakMap<Transport, Map<DescService, Client<DescService>>>();

/**
 * `service`'s client over `connection`, made once per transport. Callers
 * import the services they call, so a page's bundle holds only their schemas
 * (#252).
 */
export function client<S extends DescService>(connection: Connection, service: S): Client<S> {
  let made = clients.get(connection.transport);
  if (!made)
    clients.set(connection.transport, (made = new Map<DescService, Client<DescService>>()));
  let found = made.get(service);
  if (!found) made.set(service, (found = createClient(service, connection.transport)));
  return found as Client<S>;
}

/** Sends the session's token on every call. */
export function bearer(token: string): Interceptor {
  return (next) => (request) => {
    request.header.set("authorization", `Bearer ${token}`);
    return next(request);
  };
}

/** gRPC-web, binary only, to `baseUrl` (this page's origin by default). */
export function browserConnection(token: string, baseUrl: string = location.origin): Connection {
  const transport = createGrpcWebTransport({
    baseUrl,
    useBinaryFormat: true,
    interceptors: [bearer(token)],
  });
  return { transport, label: baseUrl };
}
