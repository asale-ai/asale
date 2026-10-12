// Is there a newer build? — asked once for the whole app, not once per page.
//
// Two places need the answer and they are never open at the same time: the
// Settings page, where the update is acted on, and the sidebar, whose whole job
// here is to tell someone who is *not* on Settings that there is a reason to go
// there. A hook with its own state in each of them would check twice and
// disagree while one of them was unmounted, so the state lives in this module
// and the components subscribe to it.
//
// The loop is deliberately quiet. It starts a few seconds after launch (never
// before: the daemon handshake the whole UI waits on is happening then, and a
// release feed is not worth a millisecond of it) and repeats every ten minutes.
// Every check reschedules the next one, so pressing "check for updates" in
// Settings resets the clock rather than racing the timer that was already
// running.
//
// What counts as "newer" comes from asale.ai's release manifest — the same
// document install.sh and install.ps1 read. Asking the same file is the point:
// the native updater stages both the app and CLI from this release before
// handing over to a helper. A protected install keeps the seller running.
//
// Desktop shell only. `realTauri`, not `inTauri` — the latter is always true;
// a browser pointed at a remote daemon cannot install anything onto the machine
// it is looking at, and the manifest sends no CORS header for it to read.

import { useEffect, useState, useSyncExternalStore } from "react";
import { invoke, realTauri } from "../lib";
import { onUpdateProgress, shell } from "../shell";
import { errText } from "../errors";

export type CheckPhase = "idle" | "checking" | "none" | "available" | "error";

export interface UpdateState {
  phase: CheckPhase;
  /**
   * Is there a newer release to install?
   *
   * Its own field rather than `phase === "available"` so that a check which
   * *fails* — the machine went offline for a minute — leaves the answer where
   * the last successful one put it. Losing a marker the user has already seen,
   * because of a blip they did not, reads as the update having gone away.
   */
  available: boolean;
  /** This build. Empty until the first check has run. */
  current: string;
  /** What asale.ai publishes. Empty when the check has not answered. */
  latest: string;
  /** The release page, for "what changed?". */
  page: string;
  error: string;
  /** `Date.now()` of the last completed check; 0 = never checked. */
  checkedAt: number;
  autoUpdate: boolean;
}

/** How often the background loop asks. */
const INTERVAL_MS = 10 * 60 * 1000;
/** How long after launch the first check waits. */
const FIRST_DELAY_MS = 5000;

const EMPTY: UpdateState = {
  phase: "idle",
  available: false,
  current: "",
  latest: "",
  page: "",
  error: "",
  checkedAt: 0,
  autoUpdate: true,
};

let state: UpdateState = EMPTY;
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setTimeout> | null = null;
let inflight: Promise<void> | null = null;
let watching = false;

function set(patch: Partial<UpdateState>) {
  state = { ...state, ...patch };
  for (const l of listeners) l();
}

/** The current answer outside React, for code that has just awaited a check and
 *  needs what it produced rather than the render it has closed over. */
export function getUpdateState(): UpdateState {
  return state;
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

/** Subscribe a component to the shared answer. */
export function useUpdateState(): UpdateState {
  return useSyncExternalStore(subscribe, getUpdateState, getUpdateState);
}

/**
 * Is there something to tell the user about?
 *
 * True from the moment a newer release is found until the app is restarted onto
 * it — which, since the installer replaces this binary and reopens the app, is
 * the same thing as "until the update has been applied".
 */
export function hasPendingUpdate(s: UpdateState): boolean {
  return s.available;
}

/**
 * Is `latest` newer than `current`?
 *
 * Segment by segment as numbers, because 0.2.10 is ahead of 0.2.9 and a string
 * comparison says the opposite. Equal numbers with a prerelease suffix on this
 * build (`0.3.0-rc1`) count as behind the plain release of the same number, and
 * nothing else counts as newer — a machine running something *ahead* of the
 * feed (a local build) must not be nagged to downgrade.
 */
export function isNewer(latest: string, current: string): boolean {
  if (!latest || !current) return false;
  const nums = (v: string) =>
    v.trim().replace(/^v/i, "").split("-")[0].split(".").map((n) => Number(n) || 0);
  const a = nums(latest);
  const b = nums(current);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const x = a[i] ?? 0;
    const y = b[i] ?? 0;
    if (x !== y) return x > y;
  }
  return current.includes("-") && !latest.includes("-");
}

/**
 * Ask the release manifest, now.
 *
 * Safe to call from anywhere at any time: concurrent callers join the check
 * that is already running.
 */
export function checkForUpdate(): Promise<void> {
  if (!realTauri) return Promise.resolve();
  if (inflight) return inflight;

  set({ phase: "checking", error: "" });
  inflight = (async () => {
    try {
      const preference = await invoke<string | null>("get_setting", { key: "auto_update" });
      set({ autoUpdate: preference !== "0" });
      const { getVersion } = await import("@tauri-apps/api/app");
      const [current, release] = await Promise.all([getVersion(), shell.latestRelease()]);
      const latest = release?.version ?? "";
      const available = isNewer(latest, current);
      set({
        phase: available ? "available" : "none",
        available,
        current,
        latest,
        page: release?.page ?? "",
        checkedAt: Date.now(),
        error: "",
      });
      const automaticError = await invoke<string | null>("get_setting", { key: "auto_update_error" });
      if (automaticError) setInstall({ phase: "idle", error: automaticError });
    } catch (e) {
      // A failed check is not news — the machine may simply be offline. It is
      // recorded for the Settings page to show if the user is looking, and the
      // sidebar marker stays as it was rather than flapping on and off.
      set({ phase: "error", error: errText(e), checkedAt: Date.now() });
    } finally {
      inflight = null;
      schedule();
    }
  })();
  return inflight;
}

export async function loadUpdatePreferences(): Promise<void> {
  const [preference, error] = await Promise.all([
    invoke<string | null>("get_setting", { key: "auto_update" }),
    invoke<string | null>("get_setting", { key: "auto_update_error" }),
  ]);
  set({ autoUpdate: preference !== "0" });
  if (error) setInstall({ phase: "idle", error });
}

export async function setAutoUpdate(enabled: boolean): Promise<void> {
  await invoke("set_setting", { key: "auto_update", value: enabled ? "1" : "0" });
  set({ autoUpdate: enabled });
}

function schedule() {
  if (!watching) return;
  if (timer !== null) clearTimeout(timer);
  timer = setTimeout(() => {
    void checkForUpdate();
  }, INTERVAL_MS);
}

/**
 * Start the background loop. Idempotent, and never awaited by anything —
 * launching the app must not wait on a network round trip to a release feed.
 */
export function startUpdateWatcher(): void {
  if (!realTauri || watching) return;
  watching = true;
  timer = setTimeout(() => {
    void checkForUpdate();
  }, FIRST_DELAY_MS);
}

// ── applying it ─────────────────────────────────────────────────────────────

/**
 * Where the update has got to.
 *
 * `downloading` and `installing` are two states rather than one "busy" because
 * they cost the user completely different things: the first is a progress bar
 * they can watch and cancel by walking away, the second closes the app and asks
 * for a password. Showing one label for both is how "update" became a button
 * that made the window disappear.
 */
export type InstallPhase = "idle" | "confirming" | "downloading" | "installing";

/** What "restart to update" needs from a component that renders it. */
export interface Installer {
  /** The exact command that will run. Empty until the shell has answered. */
  command: string;
  phase: InstallPhase;
  /** Has the user been shown what this costs and not yet agreed? */
  confirming: boolean;
  /** Downloading or installing — nothing else should be pressable. */
  running: boolean;
  /** 0–1, or `null` while no total is known (before the first event, or when
   *  the manifest carried no sizes). */
  progress: number | null;
  /** Bytes so far and expected, for the "12.4 / 33.6 MB" under the bar. */
  received: number;
  total: number;
  error: string;
  ask(): void;
  cancel(): void;
  run(): void;
}

/**
 * One update in flight for the whole app, not one per component.
 *
 * The forced-upgrade dialog and the Settings card are both mounted at once, and
 * a user who starts the download from one and then opens the other must see the
 * same bar rather than a second idle button offering to start it again. Same
 * reasoning as the release check above, and it is why this state lives in the
 * module and the hook only subscribes.
 */
interface InstallState {
  phase: InstallPhase;
  received: number;
  total: number;
  error: string;
}

let install: InstallState = { phase: "idle", received: 0, total: 0, error: "" };
const installListeners = new Set<() => void>();

function setInstall(patch: Partial<InstallState>) {
  install = { ...install, ...patch };
  for (const l of installListeners) l();
}

function getInstall(): InstallState {
  return install;
}

/** Subscribe to the shared download and safe handover state. */
export function useInstaller(): Installer {
  const [command, setCommand] = useState("");
  const state = useSyncExternalStore(
    (cb) => {
      installListeners.add(cb);
      return () => installListeners.delete(cb);
    },
    getInstall,
    getInstall,
  );

  useEffect(() => {
    // Asked, not assembled here: the shell builds it from the same constants
    // its helper script uses, so what is shown cannot drift from what runs.
    let alive = true;
    shell.installerCommand()
      .then((c) => { if (alive) setCommand(c ?? ""); })
      .catch(() => {});
    return () => { alive = false; };
  }, []);

  return {
    command,
    phase: state.phase,
    confirming: state.phase === "confirming",
    running: state.phase === "downloading" || state.phase === "installing",
    progress: state.total > 0 ? Math.min(1, state.received / state.total) : null,
    received: state.received,
    total: state.total,
    error: state.error,
    ask: () => setInstall({ phase: "confirming", error: "" }),
    cancel: () => setInstall({ phase: "idle" }),
    run: () => void startUpdate(),
  };
}

/**
 * Download, then hand over. Safe to call twice — the second call joins the first
 * rather than starting a parallel download of the same file.
 */
let updating: Promise<void> | null = null;

export function startUpdate(): Promise<void> {
  // Nothing to install onto this machine from a browser, and the shell calls
  // below would resolve to `null` — leaving the UI stuck on "installing…" for a
  // handover that never happened.
  if (!realTauri) return Promise.resolve();
  if (updating) return updating;
  updating = (async () => {
    setInstall({ phase: "downloading", received: 0, total: 0, error: "" });
    const off = onUpdateProgress((p) => setInstall({ received: p.received, total: p.total }));
    try {
      await shell.downloadUpdate();
    } catch (e) {
      // Stop here, with the window still up to say why.
      setInstall({ phase: "idle", error: errText(e) });
      return;
    } finally {
      off();
    }
    // No success branch to render past this point: if it works, the window is
    // gone within the second and a new one opens when the install finishes.
    // Only the failure to hand off to the helper ever comes back here.
    setInstall({ phase: "installing" });
    try {
      await shell.runInstaller();
    } catch (e) {
      setInstall({ phase: "idle", error: errText(e) });
    }
  })().finally(() => {
    updating = null;
  });
  return updating;
}
