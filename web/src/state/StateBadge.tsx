// A state as a shape and a word, never as color alone.
import type { State } from "./states";

const R = 5.5;

/** A state's shape in a 14 × 14 box, drawn in `currentColor`. */
export function StateShape({ state }: { state: State }) {
  const stroke = { fill: "none", stroke: "currentColor", strokeWidth: 1.5 };
  switch (state) {
    case "draft":
      return <circle cx="7" cy="7" r={R} {...stroke} />;
    case "proposed":
      return (
        <>
          <circle cx="7" cy="7" r={R} {...stroke} />
          <path d={`M7 ${7 - R} A${R} ${R} 0 0 1 7 ${7 + R} Z`} fill="currentColor" />
        </>
      );
    case "queued":
      return (
        <>
          <circle cx="7" cy="7" r={R} {...stroke} />
          <circle cx="7" cy="7" r="2" fill="currentColor" />
        </>
      );
    case "testing":
      return (
        <path
          d={`M7 7 L7 ${7 - R} A${R} ${R} 0 1 0 ${7 + R} 7 Z`}
          fill="currentColor"
          stroke="currentColor"
          strokeWidth={1.5}
          strokeLinejoin="round"
        />
      );
    case "landed":
      return (
        <>
          <circle cx="7" cy="7" r={R + 0.75} fill="currentColor" />
          <path
            d="M4.5 7.2 6.3 9 9.7 5.3"
            fill="none"
            stroke="var(--surface)"
            strokeWidth={1.6}
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </>
      );
    case "failed":
      return (
        <>
          <circle cx="7" cy="7" r={R + 0.75} fill="currentColor" />
          <path
            d="M5 5 9 9M9 5 5 9"
            stroke="var(--surface)"
            strokeWidth={1.6}
            strokeLinecap="round"
          />
        </>
      );
    case "conflicted":
      return <path d="M7 1 13 7 7 13 1 7Z" fill="currentColor" />;
    case "ended":
      return <circle cx="7" cy="7" r={R} {...stroke} strokeDasharray="2.2 1.8" />;
    case "none":
      return null;
  }
}

/**
 * A state's shape and word. With `word={false}` the word is only the badge's
 * `title`: for draft rows in the smartlog (R2.19 decision 3), where the row's
 * accessible name carries it.
 */
export function StateBadge({
  state,
  label,
  word = true,
}: {
  state: State;
  label?: string;
  word?: boolean;
}) {
  const text = label ?? state;
  return (
    <span
      className="state-badge"
      style={{ color: `var(--state-${state})` }}
      data-state={state}
      title={word ? undefined : text}
    >
      {state !== "none" && (
        <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true" focusable="false">
          <StateShape state={state} />
        </svg>
      )}
      {word && <span>{text}</span>}
    </span>
  );
}
