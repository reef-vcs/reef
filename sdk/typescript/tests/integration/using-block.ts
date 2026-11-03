// The workspace block under `await using`, which Node 24 and later parse (R2.07's test 20).

import { writeFileSync } from "node:fs";
import { join } from "node:path";

import type { Client } from "../../src/index.js";

/** Holds a workspace with `await using`, writes a file, and returns its id. */
export async function usingBlock(client: Client, repo: string): Promise<string> {
  await using ws = await client.workspace({ repo, purpose: "await using" });
  writeFileSync(join(ws.path, "using.txt"), "await using\n");
  return ws.id;
}
