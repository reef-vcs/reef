// The settings button and its panel (the design pass): the developer-mode
// switch, the daemon's version, and the links the footer used to hold. A
// plain disclosure: Escape and a click outside close it, as does moving to
// another page.
import { useEffect, useId, useRef, useState } from "react";
import type { GetDaemonStatusResponse } from "../gen/reef/api/v1/daemon_pb";
import { setDevMode, useDevMode } from "./dev-mode";
import { SettingsIcon } from "./icons";
import { Link, usePath } from "./router";

export function Settings({ status }: { status: GetDaemonStatusResponse | null }) {
  // Open on the page it was opened on: moving to another page closes it.
  const path = usePath();
  const [openOn, setOpenOn] = useState<string | null>(null);
  // Forget it once the page changes, so Back does not reopen it.
  if (openOn !== null && openOn !== path) setOpenOn(null);
  const open = openOn === path;
  const setOpen = (next: boolean) => {
    setOpenOn(next ? path : null);
  };
  const dev = useDevMode();
  const panelId = useId();
  const box = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    function onKey(event: KeyboardEvent) {
      if (event.key !== "Escape") return;
      setOpenOn(null);
      button.current?.focus();
    }
    function onPointer(event: PointerEvent) {
      if (box.current && !box.current.contains(event.target as Node)) setOpenOn(null);
    }
    document.addEventListener("keydown", onKey);
    document.addEventListener("pointerdown", onPointer);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("pointerdown", onPointer);
    };
  }, [open]);

  return (
    <div className="settings" ref={box}>
      <button
        ref={button}
        type="button"
        className="icon-button"
        aria-label="Settings"
        aria-expanded={open}
        aria-controls={panelId}
        onClick={() => {
          setOpen(!open);
        }}
      >
        <SettingsIcon />
      </button>
      {open && (
        <div className="settings-panel" id={panelId} role="group" aria-label="Settings">
          <div className="setting">
            <div>
              <div className="setting-name" id={`${panelId}-dev`}>
                Developer mode
              </div>
              <div className="secondary">Show ids, CLI commands, revsets and exact times.</div>
            </div>
            <button
              type="button"
              role="switch"
              className="switch"
              aria-checked={dev}
              aria-labelledby={`${panelId}-dev`}
              onClick={() => {
                setDevMode(!dev);
              }}
            />
          </div>
          <div className="settings-about">
            <h2>About</h2>
            {status ? (
              <p className="secondary">
                reef-daemon {status.version} · API revision {status.apiRevision}
              </p>
            ) : (
              <p className="secondary">Reading the daemon’s version.</p>
            )}
            <p className="settings-links">
              <Link to="/states">State colors</Link>
              <a href="/THIRD_PARTY_LICENSES.txt">Third-party licenses</a>
            </p>
          </div>
        </div>
      )}
    </div>
  );
}
