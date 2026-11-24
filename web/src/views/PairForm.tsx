import { useState, type SubmitEvent } from "react";
import type { Connection } from "../api/connection";
import { reefError } from "../api/errors";
import { CODE_LENGTH, pair, pairingWords, typedCode } from "../pair";

/** The pairing code's field (R3.40). */
export function PairForm({
  onPaired,
  connect,
}: {
  onPaired: (token: string) => void;
  connect?: ((code: string) => Connection) | undefined;
}) {
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState("");
  const code = typedCode(typed);
  function submit(event: SubmitEvent<HTMLFormElement>) {
    event.preventDefault();
    if (busy || code.length !== CODE_LENGTH) return;
    setBusy(true);
    setSaid("");
    pair(code, connect).then(onPaired, (thrown: unknown) => {
      setBusy(false);
      setSaid(pairingWords(reefError(thrown)));
    });
  }
  return (
    <form className="pair-form" onSubmit={submit}>
      <p>
        On the computer Reef runs on, run <code>reef remote pair</code> with a name for this device,
        and type the code it prints.
      </p>
      <label>
        Pairing code
        <input
          value={typed}
          placeholder="XXXX-XXXX"
          autoComplete="off"
          autoCapitalize="characters"
          autoCorrect="off"
          spellCheck={false}
          onChange={(event) => {
            setTyped(event.target.value);
          }}
        />
      </label>
      <p className="actions">
        <button type="submit" className="primary" disabled={busy || code.length !== CODE_LENGTH}>
          Pair
        </button>
      </p>
      {busy && <p className="secondary">Pairing.</p>}
      {said && <p role="alert">{said}</p>}
    </form>
  );
}
