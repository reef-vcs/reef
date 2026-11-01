// One row of the smartlog's graph as its own small SVG (R2.19): the lanes
// passing it, the lanes arriving and leaving, and the node, drawn as the
// change's state shape in the state's color. Lines use `--text-secondary`
// (at least 4.5:1 in both themes); only the node takes a state color. The
// graph is decoration: the row's accessible name says what it shows.
import type { GraphRow, LaneKind, Layout } from "../graph/layout";
import type { State } from "../state/states";
import { StateShape } from "../state/StateBadge";

export const ROW_HEIGHT = 28;
export const LANE_WIDTH = 14;
const MID = ROW_HEIGHT / 2;
const NODE = 7;

const x = (column: number) => LANE_WIDTH / 2 + column * LANE_WIDTH;

function dash(kind: LaneKind | "stub"): string | undefined {
  return kind === "indirect" ? "3 3" : kind === "stub" ? "1.5 2" : undefined;
}

function Line({
  from,
  to,
  kind,
}: {
  from: [number, number];
  to: [number, number];
  kind: LaneKind | "stub";
}) {
  const [x1, y1] = from;
  const [x2, y2] = to;
  // A lane that changes column bends half way, as jj's and Sapling's do.
  const d =
    x1 === x2
      ? `M${String(x1)} ${String(y1)}V${String(y2)}`
      : `M${String(x1)} ${String(y1)}C${String(x1)} ${String((y1 + y2) / 2)} ${String(x2)} ${String((y1 + y2) / 2)} ${String(x2)} ${String(y2)}`;
  return <path d={d} fill="none" strokeDasharray={dash(kind)} />;
}

function svgProps(width: number) {
  const w = Math.max(width, 1) * LANE_WIDTH;
  return {
    className: "graph",
    width: w,
    height: ROW_HEIGHT,
    viewBox: `0 0 ${String(w)} ${String(ROW_HEIGHT)}`,
    "aria-hidden": true,
    focusable: false,
  } as const;
}

export function GraphCell({ row, width, state }: { row: GraphRow; width: number; state: State }) {
  const cx = x(row.column);
  return (
    <svg {...svgProps(width)}>
      <g stroke="var(--text-secondary)" strokeWidth={1.5}>
        {row.passing.map((p) => (
          <Line
            key={`p${String(p.column)}`}
            from={[x(p.column), 0]}
            to={[x(p.column), ROW_HEIGHT]}
            kind={p.kind}
          />
        ))}
        {row.incoming && <Line from={[cx, 0]} to={[cx, MID - NODE]} kind={row.incoming} />}
        {row.joins.map((j) => (
          <Line
            key={`j${String(j.from)}`}
            from={[x(j.from), 0]}
            to={[cx, MID - NODE / 2]}
            kind={j.kind}
          />
        ))}
        {row.edges.map((e) => (
          <Line
            key={`e${String(e.to)}${e.kind}`}
            from={[cx, MID + NODE / (e.to === row.column ? 1 : 2)]}
            to={[x(e.to), ROW_HEIGHT]}
            kind={e.kind}
          />
        ))}
        {row.stub && (
          <>
            <Line from={[cx, MID + NODE]} to={[cx, ROW_HEIGHT - 3]} kind="stub" />
            <path d={`M${String(cx - 3)} ${String(ROW_HEIGHT - 3)}h6`} />
          </>
        )}
      </g>
      <g
        transform={`translate(${String(cx - NODE)} ${String(MID - NODE)})`}
        style={{ color: `var(--state-${state})` }}
      >
        <StateShape state={state} />
      </g>
    </svg>
  );
}

/** The lanes still open after the last row, fading out: parents beyond the cut. */
export function OpenLanes({ open, width }: { open: Layout["open"]; width: number }) {
  return (
    <svg {...svgProps(width)}>
      <g stroke="var(--text-secondary)" strokeWidth={1.5} className="fade">
        {open.map((lane) => (
          <Line
            key={lane.column}
            from={[x(lane.column), 0]}
            to={[x(lane.column), MID]}
            kind={lane.kind}
          />
        ))}
      </g>
    </svg>
  );
}
