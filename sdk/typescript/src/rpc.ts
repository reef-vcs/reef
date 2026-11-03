// One RPC's description, as src/gen/rpcs.ts lists them.

import type { DescMessage, DescMethodServerStreaming, DescMethodUnary } from "@bufbuild/protobuf";

/** `(reef.api.v1.access)`: a remote daemon serves READ and WRITE only. */
export type Access = "READ" | "WRITE" | "ADMIN" | "UNSPECIFIED";

/** An RPC of `reef.api.v1`: its path, descriptor, access level and paging. */
export interface Rpc<I extends DescMessage = DescMessage, O extends DescMessage = DescMessage> {
  /** The method's name in the schema, `CreateWorkspace`. */
  readonly name: string;
  /** The gRPC method path, `/reef.api.v1.<Service>/<Method>`. */
  readonly path: string;
  readonly method: DescMethodUnary<I, O> | DescMethodServerStreaming<I, O>;
  readonly access: Access;
  readonly kind: "unary" | "server_streaming";
  /** Whether it takes `pageToken` and answers `nextPageToken`. */
  readonly paged: boolean;
  /** The response's repeated field that `<method>All` yields, for a paged RPC. */
  readonly items: string | null;
}
