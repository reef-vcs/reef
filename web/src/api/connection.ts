// How the app reaches a daemon. The browser build talks gRPC-web to the
// listener that served it; a shell (R3.39, R3.40) or a store (R4.13)
// supplies its own `Connection` instead, so nothing else in the app assumes
// the API is same-origin.
import type { DescService } from "@bufbuild/protobuf";
import { createClient, type Client, type Interceptor, type Transport } from "@connectrpc/connect";
import { createGrpcWebTransport } from "@connectrpc/connect-web";
import { isSessionInvalid, reefError, type ReefError } from "./errors";
import { steady } from "./steady";
import { streamsOf } from "./streams";

export interface Connection {
  transport: Transport;
  /** Where the connection goes, for display. */
  label: string;
  /** The daemon's last refusal of the page's token, for why the session ended. */
  refused?: ReefError;
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

/** Keeps each refusal of the token, of a call or of a stream, as `connection.refused`. */
export function noteRefusals(connection: () => Connection | undefined): Interceptor {
  const noted = (thrown: unknown) => {
    const error = reefError(thrown);
    const to = connection();
    if (to && isSessionInvalid(error)) to.refused = error;
    return thrown;
  };
  async function* each<T>(messages: AsyncIterable<T>): AsyncGenerator<T> {
    try {
      yield* messages;
    } catch (thrown) {
      throw noted(thrown);
    }
  }
  return (next) => async (request) => {
    try {
      const response = await next(request);
      return response.stream ? { ...response, message: each(response.message) } : response;
    } catch (thrown) {
      throw noted(thrown);
    }
  };
}

/**
 * gRPC-web, binary only, to `baseUrl` (this page's origin by default). Over
 * https, the remote listener, what reads survives a connection that dies.
 */
export function browserConnection(
  token: string,
  baseUrl: string = location.origin,
  remote = location.protocol === "https:",
): Connection {
  const reads = remote ? steady(() => streamsOf(made)) : undefined;
  const transport = createGrpcWebTransport({
    baseUrl,
    useBinaryFormat: true,
    interceptors: [...(reads ? [reads.interceptor] : []), noteRefusals(() => made), bearer(token)],
    ...(reads ? { fetch: reads.fetch } : {}),
  });
  const made: Connection = { transport, label: baseUrl };
  return made;
}
