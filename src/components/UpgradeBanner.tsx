// The platform has stopped trading with this build — say so where the user
// cannot miss it, and put the fix one click away.
//
// Rendered above the whole app rather than on a page, because the two things it
// blocks (buying, selling) are driven from two different pages and from outside
// the window entirely: a user whose Codex session just stopped working has no
// reason to open Settings and look for an update.
//
// One button, doing the whole job — check, download, install, relaunch — since
// the user has already been told what is wrong and asking them to then find the
// update flow themselves is the step where people give up. Outside the desktop
// shell (a browser pointed at a remote daemon) there is nothing to install from
// here, so it degrades to the download link.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
// `realTauri`, not `inTauri` — the latter is a compatibility shim that is
// always true. The updater plugin only exists in the desktop shell, and a
// browser pointed at a remote daemon must fall back to the download link.
import { invoke, realTauri } from "../lib";
import { openExternal } from "../shell";
import { SITE_URL } from "../links";
import { errText } from "../errors";

/** What the daemon reports; `null` while the platform is happy with us. */
export interface UpgradeNotice {
  current: string;
  min: string;
  /** Which path was refused — "sell" or "buy". */
  path: string;
}

type Phase = "idle" | "checking" | "downloading" | "ready" | "error";

export function UpgradeBanner({ notice }: { notice: UpgradeNotice }) {
  const { t } = useTranslation();
  const [phase, setPhase] = useState<Phase>("idle");
  const [progress, setProgress] = useState(-1);
  const [err, setErr] = useState("");

  async function upgradeNow() {
    setErr("");
    setPhase("checking");
    try {
      const u: Update | null = await check();
      if (!u) {
        // The platform wants a version the update feed does not offer yet —
        // a rollout still in progress, or a floor set above what was published.
        // Saying so beats a silent no-op.
        setErr(t("upgrade.noneAvailable"));
        setPhase("error");
        return;
      }
      setPhase("downloading");
      setProgress(-1);
      let total = 0;
      let received = 0;
      await u.downloadAndInstall((ev) => {
        if (ev.event === "Started") {
          total = ev.data.contentLength ?? 0;
          setProgress(total > 0 ? 0 : -1);
        } else if (ev.event === "Progress") {
          received += ev.data.chunkLength;
          if (total > 0) setProgress(Math.min(100, Math.round((received / total) * 100)));
        } else if (ev.event === "Finished") {
          setProgress(100);
        }
      });
      setPhase("ready");
    } catch (e) {
      setErr(errText(e));
      setPhase("error");
    }
  }

  const reason = notice.path === "sell" ? t("upgrade.reasonSell") : t("upgrade.reasonBuy");

  return (
    <div className="callout danger" role="alert" style={{ margin: "0 0 12px" }}>
      <div style={{ flex: 1 }}>
        <strong>{t("upgrade.title")}</strong>
        <div className="text-sm" style={{ marginTop: 4 }}>
          {notice.min
            ? t("upgrade.body", { current: notice.current, min: notice.min })
            : t("upgrade.bodyNoVersion", { current: notice.current })}{" "}
          {reason}
        </div>
        {err && (
          <div className="text-sm" style={{ marginTop: 4 }}>
            {err} —{" "}
            <a href={SITE_URL} onClick={(e) => { e.preventDefault(); openExternal(SITE_URL); }}>
              {t("upgrade.manual")}
            </a>
          </div>
        )}
      </div>
      <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
        {!realTauri ? (
          <a className="btn btn-sm" href={SITE_URL} target="_blank" rel="noreferrer">
            {t("upgrade.download")}
          </a>
        ) : phase === "ready" ? (
          <button type="button" className="btn btn-sm" onClick={() => relaunch()}>
            {t("upgrade.restart")}
          </button>
        ) : (
          <button
            type="button"
            className="btn btn-sm"
            onClick={upgradeNow}
            disabled={phase === "checking" || phase === "downloading"}
          >
            {phase === "checking"
              ? t("upgrade.checking")
              : phase === "downloading"
                ? progress >= 0
                  ? t("upgrade.downloadingPct", { pct: progress })
                  : t("upgrade.downloading")
                : t("upgrade.now")}
          </button>
        )}
      </div>
    </div>
  );
}

/**
 * Poll the daemon for an outstanding refusal.
 *
 * Slower than the profile poll next to it: this state changes when an operator
 * moves a platform-wide floor, not when the user does anything, and the banner
 * appearing a few seconds later costs nothing.
 */
export function useUpgradeNotice(enabled: boolean): UpgradeNotice | null {
  const [notice, setNotice] = useState<UpgradeNotice | null>(null);
  useEffect(() => {
    if (!enabled) return;
    let alive = true;
    const poll = () =>
      invoke<UpgradeNotice | null>("upgrade_notice")
        .then((n) => {
          if (alive) setNotice(n ?? null);
        })
        // A daemon that is down cannot be refused by anything; leave the banner
        // as it was rather than flapping it on every failed poll.
        .catch(() => {});
    poll();
    const id = setInterval(poll, 15000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [enabled]);
  return notice;
}
