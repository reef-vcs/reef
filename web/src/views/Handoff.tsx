import { NO_SUPPORT } from "../sign/passkey";
import { embedded } from "../session";

export function NoPasskeysHere() {
  if (!embedded()) return <p className="secondary">{NO_SUPPORT}</p>;
  return (
    <p className="notice">
      Passkeys run in your browser.{" "}
      <a href={location.pathname} target="_blank" rel="noopener noreferrer">
        Continue in your browser
      </a>
    </p>
  );
}
