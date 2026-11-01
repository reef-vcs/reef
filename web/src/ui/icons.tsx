// Hand-written line icons on a 16 px grid, drawn in `currentColor` and
// sized by CSS. Decoration only: each sits beside a word that names it.
import type { ReactNode } from "react";

function Icon({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <svg
      className={className ? `icon ${className}` : "icon"}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.5}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {children}
    </svg>
  );
}

export function SettingsIcon() {
  return (
    <Icon>
      <path d="M2 4.5h7.5M12.5 4.5H14M2 11.5h2.5M7.5 11.5H14" />
      <circle cx="11" cy="4.5" r="1.5" />
      <circle cx="6" cy="11.5" r="1.5" />
    </Icon>
  );
}

export function HistoryIcon() {
  return (
    <Icon>
      <circle cx="8" cy="8" r="6" />
      <path d="M8 4.5V8l2.5 1.5" />
    </Icon>
  );
}

export function GraphIcon() {
  return (
    <Icon>
      <circle cx="4" cy="3.5" r="1.5" />
      <circle cx="4" cy="12.5" r="1.5" />
      <circle cx="12" cy="7" r="1.5" />
      <path d="M4 5v6M4 8.5c0-1.5 1.5-1.5 6.5-1.5" />
    </Icon>
  );
}

export function LayersIcon() {
  return (
    <Icon>
      <path d="M2.5 5.5 8 2.5l5.5 3L8 8.5zM2.5 8.5 8 11.5l5.5-3M2.5 11.5 8 14.5l5.5-3" />
    </Icon>
  );
}

export function PulseIcon() {
  return (
    <Icon>
      <path d="M1.5 8h3l2-4.5 3 9 2-4.5h3" />
    </Icon>
  );
}

export function CopyIcon() {
  return (
    <Icon>
      <rect x="5.5" y="5.5" width="8" height="8" rx="1.5" />
      <path d="M10.5 5.5v-2a1 1 0 0 0-1-1h-6a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2" />
    </Icon>
  );
}

export function CheckIcon() {
  return (
    <Icon>
      <path d="M3 8.5 6.5 12 13 4.5" />
    </Icon>
  );
}

export function AlertIcon() {
  return (
    <Icon>
      <path d="M8 2 14.5 13.5h-13zM8 6.5v3.5M8 12v.01" />
    </Icon>
  );
}

export function ChevronIcon() {
  return (
    <Icon className="chevron">
      <path d="M6 3.5 10.5 8 6 12.5" />
    </Icon>
  );
}
