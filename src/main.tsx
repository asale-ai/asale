import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./App";
import { TrayPanel } from "./components/TrayPanel";
import "./styles.css";
import "./i18n";
import { initLanguage } from "./i18n";
import { initTheme } from "./theme";

// One bundle, two views. The desktop shell opens its tray overview as a second
// window over the same assets (see src-tauri/src/lib.rs), so the panel shares
// this app's RPC client, theme and translations instead of being a separate
// little program that can drift out of sync with them.
//
// Two ways to say "this is the panel", because they fail in different places:
// the shell sets the global from an initialization script, which cannot be lost
// to URL normalisation on any platform; `?view=panel` is what makes the panel
// openable in a plain browser, which is the only way to look at it while it is
// being worked on.
declare global {
  interface Window { __ASALE_VIEW__?: string }
}
const panel =
  window.__ASALE_VIEW__ === "panel" ||
  new URLSearchParams(window.location.search).get("view") === "panel";
if (panel) document.documentElement.classList.add("as-panel");

// Resolve the persisted language + theme before the first paint.
Promise.allSettled([initLanguage(), initTheme()]).then(() => {
  ReactDOM.createRoot(document.getElementById("root")!).render(
    <React.StrictMode>{panel ? <TrayPanel /> : <App />}</React.StrictMode>
  );
});
