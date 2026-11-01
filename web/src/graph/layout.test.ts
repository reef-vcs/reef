// @vitest-environment node
// R2.19 test 6: golden layouts, drawn by ascii.ts (its header documents the
// format). Each graph is ordered first, as the view orders it.
import { describe, expect, it } from "vitest";
import { dag } from "../test/dag";
import { logEntries, type CliLogEntry } from "../test/log-json";
import { reverseHex } from "../ui/ids";
import { ascii } from "./ascii";
import fixture from "./fixtures/reef-smartlog.json";
import { layout } from "./layout";
import { graphNode } from "./node";
import { order } from "./order";

function draw(text: string): string {
  return "\n" + ascii(layout(order(dag(text)).nodes)) + "\n";
}

describe("layout", () => {
  it("draws a linear stack", () => {
    expect(draw("a3 a2\na2 a1\na1 t\nt!")).toMatchInlineSnapshot(`
      "
      o   a3
      o   a2
      o   a1
      o   t
      "
    `);
  });

  it("draws two stacks on one trunk commit", () => {
    expect(draw("a2 a1\na1 t\nb1 t\nt!")).toMatchInlineSnapshot(`
      "
      o     a2
      o     a1
      | o   b1
      |/
      o     t
      "
    `);
  });

  it("draws a stack on a stack", () => {
    expect(draw("b2 b1\nb1 a2\na2 a1\na1 t\nt!")).toMatchInlineSnapshot(`
      "
      o   b2
      o   b1
      o   a2
      o   a1
      o   t
      "
    `);
  });

  it("draws a two-parent merge", () => {
    expect(draw("m a b\na r\nb r\nr!")).toMatchInlineSnapshot(`
      "
      o     m
      |\\
      o |   a
      | o   b
      |/
      o     r
      "
    `);
  });

  it("draws a three-parent merge", () => {
    expect(draw("m a b c\na r\nb r\nc r\nr!")).toMatchInlineSnapshot(`
      "
      o       m
      |\\--
      o | |   a
      | o |   b
      |/  |
      |   o   c
      |--/
      o       r
      "
    `);
  });

  it("draws a criss-cross merge", () => {
    expect(draw("x a b\ny a b\na r\nb r\nr!")).toMatchInlineSnapshot(`
      "
      o       x
      |\\
      | | o   y
      |-|/
      o |     a
      | o     b
      |/
      o       r
      "
    `);
  });

  it("draws a trunk spine with indirect edges between fork points", () => {
    expect(
      draw(`
        a2 a1
        a1 t6
        b1 t4
        c1 t2
        t6! :t4
        t4! :t2
        t2! ~t1
      `),
    ).toMatchInlineSnapshot(`
      "
      o     a2
      o     a1
      o     t6
      :
      : o   b1
      :/
      o     t4
      :
      : o   c1
      :/
      o     t2
      ~
      "
    `);
  });

  it("draws a missing edge as a stub", () => {
    expect(draw("a2 a1\na1 ~t")).toMatchInlineSnapshot(`
      "
      o   a2
      o   a1
      ~
      "
    `);
  });

  it("draws both commits of a divergent change", () => {
    expect(draw("x1/x t\nx2/x t\nt!")).toMatchInlineSnapshot(`
      "
      o     x1
      | o   x2
      |/
      o     t
      "
    `);
  });

  it("draws lanes cut by the limit as open", () => {
    expect(draw("a2 a1\nb1 t2\na1 t3")).toMatchInlineSnapshot(`
      "
      o     a2
      o     a1
      | o   b1
      . .
      "
    `);
  });

  it("draws a working copy on trunk", () => {
    expect(draw("w t\nt! :s\ns! ~r")).toMatchInlineSnapshot(`
      "
      o   w
      o   t
      :
      o   s
      ~
      "
    `);
  });

  it("draws root() alone, and nothing for an empty log", () => {
    expect(draw("zzzz!")).toMatchInlineSnapshot(`
      "
      o   zzzz
      "
    `);
    expect(layout([])).toEqual({ rows: [], open: [], width: 0 });
  });

  it("draws Reef's own smartlog", () => {
    const entries = logEntries(fixture as CliLogEntry[]);
    const graph = layout(order(entries.map(graphNode)).nodes);
    const labels = new Map(
      entries.map((e) => [
        e.change?.commitId ?? "",
        `${reverseHex(e.change?.changeId ?? "").slice(0, 8)} ${e.change?.bookmarks.join(" ") ?? ""}`.trim(),
      ]),
    );
    expect(graph.rows).toHaveLength(entries.length);
    expect("\n" + ascii(graph, (row) => labels.get(row.node.id) ?? "") + "\n")
      .toMatchInlineSnapshot(`
      "
      o     vnqkvqkp
      o     yprkloqx
      | o   zxwwruqw dependabot/cargo/cargo-minor-914a316e18@origin
      |/
      | o   nukmmyzw r2.08-remote-daemon-access r2.08-remote-daemon-access@origin
      | o   vmrrzwsv
      | o   xmuszqwx
      |/
      | o   kzmpwqyw
      |/
      o     rvpsuuqk main main@origin
      | o   wmylslwl dependabot/cargo/base64-0.23.1@origin
      |/
      | o   uxoollqo dependabot/cargo/rusqlite-0.41.0@origin
      |/
      o     uvknxrwn
      :
      : o   pmtvrrnr tokens/2026-10-31-41163502205@origin
      :/
      o     kkuzwxxq
      :
      : o   ztokyunm r2.16-query-layer r2.16-query-layer@origin
      : o   prxzprwo
      : o   xvwzpqmx
      : o   pxlmzkok
      :/
      o     mnkwvtmo
      :
      : o   wltomomz fix-burst-coalesces-debounce fix-burst-coalesces-debounce@origin
      : o   rtvusxno
      :/
      o     mqwmnnxw
      :
      : o   otvvxunl pr104
      : o   zkvypqyv
      : o   mnzyvuuy
      :/
      o     tmnvptqv
      :
      : o   wkxsprzs r2.14-symbol-index-ingestion r2.14-symbol-index-ingestion@origin
      : o   pswsnomr
      :/
      o     oyruuwxp
      | o   utvwxprm issue-95-gc-on-a-timer issue-95-gc-on-a-timer@origin
      | o   tzyzpqwl
      | o   qvwrznuw
      | o   kxkooqqn
      |/
      o     xqqnmqlp
      :
      : o   nmmxzvls r2.09-operation-log-subscription r2.09-operation-log-subscription@origin
      : o   stosrtyy
      : o   qzxutprq
      :/
      o     sourxxur
      :
      : o   uusxskzx r2.03-snapshot-describe-and-undo-services
      : o   vxrrmtsl
      : o   nolmztys
      :/
      o     uymkvkwr
      :
      : o   ozuxukml
      :/
      o     snonnsox
      :
      : o   xlsrvklp
      :/
      o     uyssxwwx
      :
      : o   qqzqwwuw
      : o   swmtzkwm
      :/
      o     txpmvywu
      :
      : o   svnnyqls
      :/
      o     nuqsxuuv
      :
      : o   qnplotkx
      : o   rmuvwmuz
      : o   vnporxxm
      : o   sqwqqurp
      :/
      o     wullxvzr
      | o   vrsqqzvl
      | o   zyruqvlt
      |/
      o     vumnlolv
      | o   rmxytlzv
      | o   psszzvzr
      |/
      o     vzmvylls
      | o   lzsolpzm
      | o   plyqyovv
      |/
      o     xtlmwpys
      | o   mlytykrz
      | o   xyytkpzu
      |/
      o     pzytpsss
      ~
      "
    `);
  });
});
