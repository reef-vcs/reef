// The bootstrap: take the session from storage or `#session=`, connect,
// and render. No StrictMode: its double mount would close the live layer.
import { createRoot } from "react-dom/client";
import { browserConnection } from "./api/connection";
import { App } from "./App";
import { openSession } from "./device";
import "./styles/tokens.css";
import "./styles/base.css";

const element = document.getElementById("root");
if (element) {
  const root = createRoot(element);
  const show = (token: string | null) => {
    root.render(
      <App
        key={token ?? ""}
        session={token ? { connection: browserConnection(token), token } : null}
        onPaired={show}
      />,
    );
  };
  show(openSession());
}
