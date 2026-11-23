// The bootstrap: take the session from `#session=` (or storage), connect,
// and render. No StrictMode: its double mount would close the live layer.
import { createRoot } from "react-dom/client";
import { browserConnection } from "./api/connection";
import { App } from "./App";
import { dropForeignWorkers, storeSession, takeSession } from "./session";
import "./styles/tokens.css";
import "./styles/base.css";

const token = takeSession();
void dropForeignWorkers();
const root = document.getElementById("root");
if (root) {
  createRoot(root).render(
    <App
      session={
        token
          ? {
              connection: browserConnection(token, () => {
                storeSession(token);
              }),
              token,
            }
          : null
      }
    />,
  );
}
