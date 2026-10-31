// WCAG 2 contrast between two `#rrggbb` or `#rgb` colors, for the states page
// and the token test. The production build minifies `#ffffff` to `#fff`.

function expand(hex: string): string {
  const short = /^#?([0-9a-f])([0-9a-f])([0-9a-f])$/i.exec(hex.trim());
  if (!short) return hex.trim();
  const doubled = short.slice(1).map((c) => c + c);
  return `#${doubled.join("")}`;
}

function luminance(hex: string): number {
  const match = /^#?([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(expand(hex));
  if (!match) throw new Error(`not a #rrggbb or #rgb color: ${hex}`);
  const [r, g, b] = match.slice(1).map((part) => {
    const c = parseInt(part, 16) / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  }) as [number, number, number];
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

export function contrast(a: string, b: string): number {
  const [light, dark] = [luminance(a), luminance(b)].sort((x, y) => y - x) as [number, number];
  return (light + 0.05) / (dark + 0.05);
}
