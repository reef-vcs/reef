// R2.21 test 2: the feed's rows from records.
import { describe, expect, it } from "vitest";
import { OperationKind } from "../gen/reef/api/v1/operations_pb";
import { coalesce, RUN_GAP_MS, type Row } from "./coalesce";
import { rec, reefLog } from "./fixture";
import { newerFirst, type OpRecord } from "./record";

const MIN = 60_000;
const T = Date.UTC(2026, 10, 1, 12);

function rows(records: OpRecord[]): Row[] {
  return coalesce([...records].sort(newerFirst));
}

function ids(rows: Row[]): string[][] {
  return rows.map((row) => row.operations.map((o) => o.id));
}

const snap = (id: string, at: number, ws = "w1", agent = "ann") =>
  rec(id, at, { workspaceId: ws, agent });

describe("coalesce", () => {
  it("joins one workspace's and agent's snapshots and edits within the gap", () => {
    const out = rows([
      snap("a", T),
      rec("b", T + MIN, { kind: OperationKind.EDIT, workspaceId: "w1", agent: "ann" }),
      snap("c", T + 2 * MIN),
    ]);
    expect(ids(out)).toEqual([["c", "b", "a"]]);
    expect(out[0]?.class).toBe("work");
    expect(out[0]?.key).toBe("a");
  });

  it("joins at exactly the gap and splits one millisecond past it", () => {
    expect(ids(rows([snap("a", T), snap("b", T + RUN_GAP_MS)]))).toEqual([["b", "a"]]);
    expect(ids(rows([snap("a", T), snap("b", T + RUN_GAP_MS + 1)]))).toEqual([["b"], ["a"]]);
  });

  it("splits by workspace and by agent", () => {
    const out = rows([snap("a", T), snap("b", T + 1, "w2"), snap("c", T + 2, "w1", "bob")]);
    expect(ids(out)).toEqual([["c"], ["b"], ["a"]]);
  });

  it("is not split by other keys' operations in between", () => {
    const out = rows([
      snap("a", T),
      snap("x", T + 1, "w2"),
      snap("b", T + 2),
      snap("y", T + 3, "w2"),
    ]);
    expect(ids(out)).toEqual([
      ["y", "x"],
      ["b", "a"],
    ]);
  });

  it("is split by an event in the same workspace, not by one elsewhere", () => {
    const describe = (id: string, at: number, ws: string) =>
      rec(id, at, { kind: OperationKind.DESCRIBE, workspaceId: ws });
    expect(ids(rows([snap("a", T), describe("d", T + 1, "w1"), snap("b", T + 2)]))).toEqual([
      ["b"],
      ["d"],
      ["a"],
    ]);
    expect(ids(rows([snap("a", T), describe("d", T + 1, "w2"), snap("b", T + 2)]))).toEqual([
      ["b", "a"],
      ["d"],
    ]);
  });

  it("groups outside operations by jj workspace and user", () => {
    const ext = (id: string, at: number, jj: string, user: string) =>
      rec(id, at, { kind: OperationKind.EXTERNAL, jjWorkspace: jj, username: user, hostname: "h" });
    const out = rows([
      ext("a", T, "default", "ann"),
      ext("b", T + 1, "default", "ann"),
      ext("c", T + 2, "default", "bob"),
      ext("d", T + 3, "other", "ann"),
    ]);
    expect(ids(out)).toEqual([["d"], ["c"], ["b", "a"]]);
    expect(out[2]?.class).toBe("outside");
  });

  it("makes every event kind, and every merge, a row of its own", () => {
    const kinds = [
      OperationKind.CREATE_WORKSPACE,
      OperationKind.RELEASE_WORKSPACE,
      OperationKind.MOVE_WORKSPACE,
      OperationKind.DESCRIBE,
      OperationKind.ABANDON,
      OperationKind.UNDO,
      OperationKind.IMPORT_REFS,
      OperationKind.OTHER,
      OperationKind.UNSPECIFIED,
    ];
    const events = kinds.map((kind, i) => rec(`e${String(i)}`, T + i, { kind, workspaceId: "w1" }));
    expect(rows(events).map((r) => r.class)).toEqual(kinds.map(() => "event"));
    const merges = [0, 1].map((i) =>
      rec(`m${String(i)}`, T + i, { workspaceId: "w1", agent: "ann", parentIds: ["x", "y"] }),
    );
    expect(ids(rows(merges))).toEqual([["m1"], ["m0"]]);
    // A snapshot without a Reef workspace is an event too.
    expect(rows([rec("s", T), rec("t", T + 1)]).map((r) => r.class)).toEqual(["event", "event"]);
  });

  it("never makes the root a row", () => {
    expect(rows([rec("root", 0, { parentIds: [] }), snap("a", T)]).length).toBe(1);
  });

  it("orders rows by their newest operation and keys them by their oldest", () => {
    const out = rows([snap("a", T), snap("x", T + 1, "w2"), snap("b", T + 2)]);
    expect(out.map((r) => r.key)).toEqual(["a", "x"]);
    expect(out.map((r) => r.operations[0]?.id)).toEqual(["b", "x"]);
  });

  it("gives the same rows whatever order the records arrived in", () => {
    const log = reefLog();
    const shuffled = [...log];
    let seed = 7;
    for (let i = shuffled.length - 1; i > 0; i--) {
      seed = (seed * 1103515245 + 12345) % 2 ** 31;
      const j = seed % (i + 1);
      [shuffled[i], shuffled[j]] = [shuffled[j] as OpRecord, shuffled[i] as OpRecord];
    }
    expect(rows(shuffled)).toEqual(rows(log));
  });

  it("coalesces Reef's own log (golden)", () => {
    const out = rows(reefLog());
    const text = out.slice(0, 20).map((row) => {
      const first = row.operations[0] as OpRecord;
      const who = first.agent || `${first.username}@${first.hostname}`;
      const where = first.workspaceId || first.jjWorkspace;
      return `${row.class} ${String(row.operations.length)} ${who} ${where} ${first.id.slice(0, 8)}`;
    });
    expect({ rows: out.length, top: text }).toMatchInlineSnapshot(`
      {
        "rows": 147,
        "top": [
          "work 46 agent-1 82feef728c99d301 285c9f02",
          "event 1 agent-1 82feef728c99d301 9104ab7a",
          "outside 1 user-1@host-2 default 89931c34",
          "event 1 agent-1 de23a3fb989ef544 699160e3",
          "work 5 agent-1 de23a3fb989ef544 9df5b6e9",
          "event 1 agent-1 de23a3fb989ef544 009d7cd0",
          "outside 1 user-1@host-2 default a45c12c1",
          "outside 3 user-1@host-1 default b7ccf8ff",
          "event 1 agent-2 383f28944469fa54 49a79246",
          "work 1 agent-2 383f28944469fa54 352da5cd",
          "work 3 agent-2 383f28944469fa54 65f9685f",
          "work 34 agent-2 383f28944469fa54 c38fd37c",
          "event 1 agent-2 383f28944469fa54 fbf9b3fd",
          "outside 1 user-1@host-1 default 4ddb5220",
          "event 1 agent-2 30537757ac5ae744 2fa3e6ef",
          "work 1 agent-2 30537757ac5ae744 001a20fe",
          "work 2 agent-2 30537757ac5ae744 1a7b2318",
          "event 1 agent-2 30537757ac5ae744 bb8a6f94",
          "event 1 agent-2 1a4f0ed05705cc56 b6dd8529",
          "outside 4 user-1@host-1 default 882cc961",
        ],
      }
    `);
  });
});
