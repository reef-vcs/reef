// Request typing, checked by tsc (`npm run typecheck`), never run (test 13).

import type { Client } from "../../../src/index.js";
import { Backend } from "../../../src/api/v1.js";

export async function typed(client: Client): Promise<void> {
  await client.createWorkspace({ repo: "r", backend: Backend.APFS });
  // @ts-expect-error: no such field.
  await client.createWorkspace({ repo: "r", nope: 1 });
  // @ts-expect-error: an enum takes its own values.
  await client.createWorkspace({ repo: "r", backend: "apfs" });
  // @ts-expect-error: a oneof is { case, value }.
  await client.createWorkspace({ repo: "r", newOn: "trunk()" });
  await client.createWorkspace({ repo: "r", start: { case: "newOn", value: "trunk()" } });
}
