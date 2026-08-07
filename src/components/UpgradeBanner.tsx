// The platform has stopped trading with this build — say so where the user
// cannot miss it, and put the fix one click away.
//
// Rendered above the whole app rather than on a page, because the two things it
// blocks (buying, selling) are driven from two different pages and from outside
// the window entirely: a user whose Codex session just stopped working has no
// reason to open Settings and look for an update.
//
// The fix is the same one Settings offers, and it is the only one there is:
// re-run the published installer, which replaces the desktop app and the
// `asale` command line together. It is offered here rather than linked to,
// because sending a blocked user off to find the update flow themselves is the
// step where people give up — but it still asks first, since it closes the app
// and needs an administrator password. Outside the desktop shell (a browser
// pointed at a remote daemon) there is nothing to install onto this machine, so
// it degrades to the download link.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
// `realTauri`, not `inTauri` — the latter is a compatibility shim that is
// always true. Only the desktop shell can run an installer on this machine.
import { invoke, realTauri } from "../lib";
import { useInstaller } from "../lib/updates";
import { openExternal } from "../shell";
import { SITE_URL } from "../links";

/** What the daemon reports; `null` while the platform is happy with us. */
export interface UpgradeNotice {
  current: string;
  min: string;
  /** Which path was refused — "sell" or "buy". */
  path: string;
}

export function UpgradeBanner({ notice }: { notice: UpgradeNotice }) {
  const { t } = useTranslation();
  const installer = useInstaller();

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
        {/* What the button is about to do, shown only once it has been asked
            for — the banner is already carrying a paragraph of bad news, and
            the cost of the fix is not news until the user is considering it. */}
        {installer.confirming && (
          <div className="text-sm" style={{ marginTop: 4 }}>{t("settings.reinstallConfirm")}</div>
        )}
        {installer.error && (
          <div className="text-sm" style={{ marginTop: 4 }}>
            {t("settings.reinstallError", { msg: installer.error })} —{" "}
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
        ) : installer.confirming ? (
          <>
            <button type="button" className="btn btn-sm" onClick={installer.run} disabled={installer.running}>
              {installer.running ? t("settings.reinstallRunning") : t("settings.reinstallGo")}
            </button>
            <button type="button" className="btn btn-sm ghost" onClick={installer.cancel} disabled={installer.running}>
              {t("settings.reinstallCancel")}
            </button>
          </>
        ) : (
          <button type="button" className="btn btn-sm" onClick={installer.ask}>
            {t("upgrade.now")}
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
