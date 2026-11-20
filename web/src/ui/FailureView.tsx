// A passkey step that did not happen: the browser's refusal in words, or the
// daemon's error with its hint and `next`.
import type { Failure } from "../sign/passkey";
import { ErrorView } from "./ErrorView";
import { AlertIcon } from "./icons";

export function FailureView({ failure }: { failure: Failure }) {
  if (failure.kind === "daemon") return <ErrorView error={failure.error} />;
  return (
    <div className="error" role="alert">
      <p className="error-message">
        <AlertIcon />
        <span>{failure.words}</span>
      </p>
    </div>
  );
}
