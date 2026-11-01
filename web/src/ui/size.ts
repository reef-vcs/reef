// A byte count in the CLI's units (`reef-vcs/src/cli/render.rs`, `size`), as
// `du -h` would: `0 B`, `512 B`, `1.2 GiB`, `41 MiB` (one decimal below ten).
const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"];

export function size(bytes: bigint | number): string {
  let value = Number(bytes);
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const name = UNITS[unit] ?? "B";
  if (unit === 0) return `${String(bytes)} B`;
  return `${value < 10 ? value.toFixed(1) : value.toFixed(0)} ${name}`;
}
