// The Reef mark: a staghorn coral branch rising out of a wave. Drawn with
// strokes so it stays crisp at 20 px; only ever shown on `--ocean`.
// `public/favicon.svg` repeats the paths with literal colors.
export function ReefMark() {
  return (
    <svg
      className="reef-mark"
      viewBox="0 0 24 24"
      width={24}
      height={24}
      fill="none"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      <path
        className="mark-branch"
        strokeWidth={1.75}
        d="M12 17v-4.5M12 12.5C12 10 8 9.5 8 5.5M12 12.5c0-2.5 4-3 4-7M12 12.5V7.5M9.1 9.4 6.5 8.2M15.2 9 17.6 7.6"
      />
      <path
        className="mark-wave"
        strokeWidth={2}
        d="M3 18.5c1.5 0 3-1.5 4.5-1.5s3 1.5 4.5 1.5 3-1.5 4.5-1.5 3 1.5 4.5 1.5"
      />
    </svg>
  );
}
