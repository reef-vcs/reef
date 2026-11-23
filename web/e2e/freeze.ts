// A TCP proxy whose connections can die as a phone's do when it sleeps or
// changes networks: what is open carries nothing more either way, with no FIN
// and no RST, while a new connection works.
import { createServer, connect, type AddressInfo, type Server, type Socket } from "node:net";

export class FreezingProxy {
  private readonly frozen = new Set<Socket>();
  private readonly open = new Map<Socket, Socket>();
  private server: Server | undefined;
  private target = 0;
  /** Connections accepted so far. */
  accepted = 0;

  /** Listens on a loopback port; that port. */
  async listen(): Promise<number> {
    this.server = createServer((client) => {
      this.accepted += 1;
      const daemon = connect(this.target, "127.0.0.1");
      this.open.set(client, daemon);
      client.on("data", (data) => {
        if (!this.frozen.has(client)) daemon.write(data);
      });
      daemon.on("data", (data) => {
        if (!this.frozen.has(client)) client.write(data);
      });
      client.on("error", () => undefined);
      daemon.on("error", () => undefined);
      client.on("close", () => {
        daemon.destroy();
        this.open.delete(client);
      });
      daemon.on("close", () => {
        if (!this.frozen.has(client)) client.destroy();
      });
    });
    const server = this.server;
    await new Promise<void>((listening) => server.listen(0, "127.0.0.1", listening));
    return (server.address() as AddressInfo).port;
  }

  /** Where connections go. */
  to(port: number) {
    this.target = port;
  }

  freeze() {
    for (const client of this.open.keys()) this.frozen.add(client);
  }

  close() {
    this.server?.close();
    for (const [client, daemon] of this.open) {
      client.destroy();
      daemon.destroy();
    }
  }
}
