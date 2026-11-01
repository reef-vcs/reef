// The app bar's slots (the design pass). The shell owns the one sticky
// header but not the repository's name, which the repository frame (a lazy
// chunk) reads; the frame portals its crumb, tabs and scrubber into these
// slots. Without a provider (a frame rendered alone in a test) the parts
// render in place.
import { createContext, type ReactNode, useContext } from "react";
import { createPortal } from "react-dom";

export interface HeaderSlotElements {
  crumb: HTMLElement | null;
  tabs: HTMLElement | null;
  strip: HTMLElement | null;
}

export const HeaderSlots = createContext<HeaderSlotElements | null>(null);

/** `children` in the app bar's `slot`, or in place when there is no app bar. */
export function InSlot({
  slot,
  children,
}: {
  slot: keyof HeaderSlotElements;
  children: ReactNode;
}) {
  const slots = useContext(HeaderSlots);
  if (!slots) return <>{children}</>;
  const element = slots[slot];
  return element ? createPortal(children, element) : null;
}
