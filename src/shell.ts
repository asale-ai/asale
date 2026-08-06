// The handful of things only the desktop shell can do.
//
// Everything else in this app goes through the daemon over HTTP (`lib.ts`),
// which is what lets the same bundle run in the Tauri window, in Chrome on this
// machine, and in a browser against a headless box. These few cannot: opening
// the OS browser, raising a native window, and quitting a process are the
// shell's, and in a browser they are either meaningless or already available.
//
// Every call is guarded, so a page may call them unconditionally: in a browser
// they resolve to `null` rather than throwing, and the UI decides what to hide.

import { realTauri } from "./lib";

async function core() {
  if (!realTauri) return null;
  // Imported lazily so a browser build never pulls the Tauri API in at all.
  return await import("@tauri-apps/api/core");
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T | null> {
  const c = await core();
  if (!c) return null;
  return (await c.invoke(cmd, args)) as T;
}

export const shell = {
  /** Available at all? False in every browser, including Chrome on this machine. */
  available: realTauri,

  /** Raise the app window (and hide the tray panel, if it is what called this). */
  showMainWindow: () => call<void>("show_main_window"),

  /** Open this daemon's UI in the system browser. Returns the URL that opened. */
  openWebUi: () => call<string>("open_web_ui"),

  /** The same URL without opening it — for "copy link", and for showing the
   *  user what they are about to hand out. It carries the daemon token. */
  webUiUrl: () => call<string>("web_ui_url"),

  /** Quit for real, rather than hiding to the tray. */
  quit: () => call<void>("quit_app"),

  /** Whether closing the window hides to the tray (true) or quits (false). */
  getCloseToTray: () => call<boolean>("get_close_to_tray"),

  /** Apply immediately. Persisting is the caller's job (`set_setting`), so that
   *  the same switch works when it is flipped from a browser. */
  setCloseToTray: (value: boolean) => call<void>("set_close_to_tray", { value }),

  hidePanel: () => call<void>("hide_tray_panel"),

  /** Make the tray panel window exactly `height` CSS pixels tall and re-anchor
   *  it to the tray icon. The panel measures itself and calls this; the shell
   *  clamps, so a broken measurement cannot produce a full-screen popup. */
  resizePanel: (height: number) => call<void>("resize_panel", { height }),
};

/**
 * Open `url` outside the app.
 *
 * In a browser that is a new tab. In the desktop shell it must be the *system*
 * browser: the webview has no tabs, no address bar and no back button, so a
 * navigation that lands there strands the user inside the app with no way out —
 * and `target="_blank"` in a WKWebView simply does nothing, which is worse
 * (a link that silently ignores the click).
 */
export async function openExternal(url: string): Promise<void> {
  if (!realTauri) {
    window.open(url, "_blank", "noopener,noreferrer");
    return;
  }
  // Lazy for the same reason as `core()` above — a browser build never loads it.
  // Two gates, both in the shell's config: the `shell:allow-open` capability,
  // and `plugins.shell.open` in tauri.conf.json. The second one only accepts
  // `true` (the plugin's own http/mailto/tel regex) or a regex matching the
  // *whole* URL — the plugin wraps whatever is given in `^...$`, so a prefix
  // like "^https://" silently matches nothing and every link goes dead.
  const { open } = await import("@tauri-apps/plugin-shell");
  await open(url);
}
