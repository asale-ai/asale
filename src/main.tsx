import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./App";
import "./styles.css";
import "./i18n";
import { initLanguage } from "./i18n";
import { initTheme } from "./theme";

// Resolve the persisted language + theme before the first paint.
Promise.allSettled([initLanguage(), initTheme()]).then(() => {
  ReactDOM.createRoot(document.getElementById("root")!).render(
    <React.StrictMode>
      <App />
    </React.StrictMode>
  );
});
