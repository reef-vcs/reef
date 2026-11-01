// Nothing reorders under the cursor (R2.19, shared since R2.20): the selected
// row, or else the first row in view, keeps its place on screen across an
// update. Measured on scroll, corrected before paint. A view says where a
// row's top is (`locate`) and which row is first in view; the smartlog's rows
// are all one height, so its answers are arithmetic, not layout reads.
import { useEffect, useLayoutEffect, useRef } from "react";

type Anchor = { key: string; top: number } | null;

export interface AnchorView {
  /** A row's top relative to the viewport, or undefined when it is not shown. */
  locate: (key: string) => number | undefined;
  /** The first row in view, or undefined when there are none. */
  firstInView: () => string | undefined;
  /** Rows are this high, for the "in view" test of the selection. */
  rowHeight: (key: string) => number;
}

function measure(view: AnchorView, selected: string): Anchor {
  const top = selected ? view.locate(selected) : undefined;
  if (top !== undefined && top + view.rowHeight(selected) > 0 && top < window.innerHeight) {
    return { key: selected, top };
  }
  const first = view.firstInView();
  if (first === undefined) return null;
  const firstTop = view.locate(first);
  return firstTop === undefined ? null : { key: first, top: firstTop };
}

/**
 * Keeps the anchor in place whenever `version` changes. `view` must be stable
 * for as long as its rows are (a memo over them), since scroll listeners are
 * attached per view.
 */
export function useScrollAnchor(view: AnchorView, version: number, selected: string) {
  const anchor = useRef<Anchor>(null);

  useEffect(() => {
    const onScroll = () => {
      anchor.current = measure(view, selected);
    };
    window.addEventListener("scroll", onScroll, { passive: true });
    return () => {
      window.removeEventListener("scroll", onScroll);
    };
  }, [view, selected]);

  useLayoutEffect(() => {
    const held = anchor.current;
    const now = held ? view.locate(held.key) : undefined;
    if (held && now !== undefined) {
      const delta = now - held.top;
      if (delta !== 0) window.scrollBy(0, delta);
    }
    anchor.current = measure(view, selected);
  }, [version, view, selected]);
}
