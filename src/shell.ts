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

  /** What asale.ai publishes as the current client release. Read in the shell
   *  rather than by the page: the manifest sends no CORS header, so a `fetch`
   *  from the webview's `tauri://` origin never sees the answer. */
  latestRelease: () => call<{ version: string; page: string }>("latest_release"),

  /** The installer command "restart to update" runs, for showing it first. */
  installerCommand: () => call<string>("installer_command"),

  /** Download this platform's half of the current release, so the installer
   *  that runs after the app closes has nothing left to fetch. Resolves with
   *  the directory it landed in; progress arrives on `onUpdateProgress`. */
  downloadUpdate: () => call<string>("download_update"),

  /** Re-run the published installer to upgrade the app *and* the `asale`
   *  command line, then reopen the app. Quits this process as a side effect —
   *  the installer replaces the binary it is running from — so nothing after
   *  this call is guaranteed to run. */
  runInstaller: () => call<void>("run_installer"),

  /** Make the tray panel window exactly `height` CSS pixels tall and re-anchor
   *  it to the tray icon. The panel measures itself and calls this; the shell
   *  clamps, so a broken measurement cannot produce a full-screen popup. */
  resizePanel: (height: number) => call<void>("resize_panel", { height }),
};

/** How far the update download has got. `total` is 0 when nobody knows. */
export interface UpdateProgress {
  file: string;
  received: number;
  total: number;
}

/**
 * Subscribe to download progress. Returns an unsubscribe function — a no-op in
 * a browser, where there is no download to watch.
 *
 * Events rather than a polled command: the download runs in the shell and the
 * bar is drawn in the webview, and polling a byte counter is how a progress bar
 * ends up either stuttering or costing more than the download.
 */
export function onUpdateProgress(cb: (p: UpdateProgress) => void): () => void {
  if (!realTauri) return () => {};
  let off: (() => void) | null = null;
  let cancelled = false;
  void (async () => {
    const { listen } = await import("@tauri-apps/api/event");
    const unlisten = await listen<UpdateProgress>("update://progress", (e) => cb(e.payload));
    if (cancelled) unlisten();
    else off = unlisten;
  })();
  return () => {
    cancelled = true;
    off?.();
  };
}

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
