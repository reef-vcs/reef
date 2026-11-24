// A TCP proxy whose connections can die as a phone's do when it sleeps or
// changes networks: what is open carries nothing more either way, with no FIN
// and no RST, while a new connection works. The daemon is told of none of it:
// its side stays open, whatever the browser does with its own.
import { createServer, connect, type AddressInfo, type Server, type Socket } from "node:net";

export class FreezingProxy {
  private readonly frozen = new Set<Socket>();
  private readonly open = new Map<Socket, Socket>();
  private readonly left = new Set<Socket>();
  private server: Server | undefined;
  private target = 0;
  private frozenAt = 0;
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
        this.open.delete(client);
        if (this.frozen.delete(client)) this.left.add(daemon);
        else daemon.destroy();
      });
      daemon.on("close", () => {
        this.left.delete(daemon);
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
    this.frozenAt = Date.now();
  }

  /** How long ago the connections died. */
  get deadMs(): number {
    return Date.now() - this.frozenAt;
  }

  close() {
    this.server?.close();
    for (const daemon of this.left) daemon.destroy();
    for (const [client, daemon] of this.open) {
      client.destroy();
      daemon.destroy();
    }
  }
}
