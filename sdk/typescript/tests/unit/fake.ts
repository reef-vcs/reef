// An in-process fake daemon for unit tests: Connect's node adapter on an h2c server bound to a
// scratch socket, serving whatever routes a test gives it.

import { mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:http2";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { ConnectRouter } from "@connectrpc/connect";
import { connectNodeAdapter } from "@connectrpc/connect-node";

export interface Fake {
  socket: string;
  dir: string;
  close(): Promise<void>;
}

export async function fakeDaemon(routes: (router: ConnectRouter) => void): Promise<Fake> {
  const dir = mkdtempSync(join(tmpdir(), "reef-fake-"));
  const socket = join(dir, "daemon.sock");
  const server = createServer(
    connectNodeAdapter({ routes, grpc: true, connect: false, grpcWeb: false }),
  );
  const open = new Set<{ destroy(): void }>();
  server.on("session", (session) => {
    open.add(session);
    session.once("close", () => open.delete(session));
  });
  await new Promise<void>((done) => server.listen(socket, done));
  return {
    socket,
    dir,
    close: () =>
      new Promise<void>((done) => {
        server.close(() => {
          rmSync(dir, { recursive: true, force: true });
          done();
        });
        for (const session of open) {
          session.destroy();
        }
      }),
  };
}
