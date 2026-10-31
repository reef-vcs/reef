// `/states`: every state as a badge and as a tinted row, in both themes side
// by side, with each foreground's contrast on the surface and on its tint.
// Later views check their colors against this page.
import { useLayoutEffect, useRef, useState } from "react";
import { contrast } from "../state/contrast";
import { StateBadge } from "../state/StateBadge";
import { MEANINGS, STATES, type State } from "../state/states";

function Ratios({ state }: { state: State }) {
  const ref = useRef<HTMLSpanElement>(null);
  const [text, setText] = useState("");
  useLayoutEffect(() => {
    if (!ref.current) return;
    const style = getComputedStyle(ref.current);
    const read = (name: string) => style.getPropertyValue(name).trim();
    try {
      const fg = read(`--state-${state}`);
      const onSurface = contrast(fg, read("--surface"));
      const onTint = contrast(fg, read(`--state-${state}-tint`));
      setText(`${onSurface.toFixed(1)}:1 on the surface, ${onTint.toFixed(1)}:1 on the tint`);
    } catch {
      setText("");
    }
  }, [state]);
  return (
    <span ref={ref} className="secondary">
      {text}
    </span>
  );
}

function Theme({ theme }: { theme: "light" | "dark" }) {
  return (
    <section data-theme={theme}>
      <h3>{theme === "light" ? "Light" : "Dark"}</h3>
      <table>
        <tbody>
          {STATES.map((state) => (
            <tr key={state} style={{ background: `var(--state-${state}-tint)` }}>
              <td>
                <StateBadge state={state} />
              </td>
              <td>
                <Ratios state={state} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

export function States() {
  return (
    <>
      <h2>States</h2>
      <p className="secondary">
        One color per object state, used for every object that has it, always with its shape and its
        word. Connection status, errors and chrome never use these colors.
      </p>
      <div className="table-box">
        <table>
          <thead>
            <tr>
              <th scope="col">State</th>
              <th scope="col">Meaning</th>
            </tr>
          </thead>
          <tbody>
            {STATES.map((state) => (
              <tr key={state}>
                <td>
                  <StateBadge state={state} />
                </td>
                <td>{MEANINGS[state]}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="themes">
        <Theme theme="light" />
        <Theme theme="dark" />
      </div>
    </>
  );
}
