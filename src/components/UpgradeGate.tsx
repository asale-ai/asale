// The platform has stopped trading with this build — say so where the user
// cannot miss it, and put the fix one click away.
//
// A modal over the whole app rather than a banner above a page. It started as a
// banner, and a banner is the wrong shape for this: everything the window offers
// while it is showing — the sell switches, the buy switches, the market board —
// is either inert or about to fail, so a strip the user can scroll past is an
// invitation to spend ten minutes finding that out one click at a time. There is
// exactly one action available on this machine, and this is the app saying so.
//
// Deliberately not dismissable. No close button, no Escape, nothing behind it is
// clickable: "later" is not a state the platform offers, and a dialog that can be
// waved away teaches the user to wave it away.
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
import { useInstaller, useUpdateState } from "../lib/updates";
import { UpdateProgress } from "./UpdateProgress";
import { openExternal } from "../shell";
import { SITE_URL } from "../links";

/** What the daemon reports; `null` while the platform is happy with us. */
export interface UpgradeNotice {
  current: string;
  min: string;
  /**
   * How it was learned — "sell" or "buy" when a trade was refused, "platform"
   * when the client read the floor itself before anything was tried. Only the
   * wording depends on it: what is blocked is the same either way.
   */
  path: string;
}

export function UpgradeRequiredDialog({ notice }: { notice: UpgradeNotice }) {
  const { t } = useTranslation();
  const installer = useInstaller();
  // Whatever the release feed has already answered. Not fetched for this dialog
  // — the watcher runs from app start regardless — and not waited for either:
  // the version to install is a nice thing to name and never the thing that
  // decides whether this shows.
  const update = useUpdateState();

  const reason =
    notice.path === "sell"
      ? t("upgrade.reasonSell")
      : notice.path === "buy"
        ? t("upgrade.reasonBuy")
        : t("upgrade.reasonPlatform");

  return (
    <div className="modal-backdrop upgrade-gate">
      <div className="modal upgrade-modal" role="alertdialog" aria-modal="true" aria-labelledby="upgrade-title">
        <div className="modal-head">
          {/* No `modal-x`: see the header comment. */}
          <h3 id="upgrade-title">{t("upgrade.title")}</h3>
        </div>
        <div className="upgrade-body">
          <p>
            {notice.min
              ? t("upgrade.body", { current: notice.current, min: notice.min })
              : t("upgrade.bodyNoVersion", { current: notice.current })}
          </p>
          <p className="text-sm">{reason}</p>
          {/* Only once the feed has something newer to name. Repeating the
              current version back at someone who has just been told it is too
              old adds nothing. */}
          {update.available && update.latest && (
            <p className="text-sm">{t("upgrade.latest", { version: update.latest })}</p>
          )}
          {/* What the button is about to do, shown only once it has been asked
              for — the dialog is already carrying a paragraph of bad news, and
              the cost of the fix is not news until the user is considering it. */}
          {installer.confirming && (
            <p className="text-sm">{t("settings.reinstallConfirm")}</p>
          )}
          {/* The same bar the Settings card draws, from the same state: an
              update started in one place is watchable from the other. */}
          <UpdateProgress installer={installer} />
          {installer.error && (
            <p className="text-sm">
              {t("settings.reinstallError", { msg: installer.error })} —{" "}
              <a href={SITE_URL} onClick={(e) => { e.preventDefault(); openExternal(SITE_URL); }}>
                {t("upgrade.manual")}
              </a>
            </p>
          )}
        </div>
        <div className="modal-foot">
          <div className="modal-spacer" />
          {!realTauri ? (
            <a className="btn" href={SITE_URL} target="_blank" rel="noreferrer">
              {t("upgrade.download")}
            </a>
          ) : installer.running ? (
            // No cancel: the bar under it is the whole story, and a button that
            // aborts a download halfway leaves the user exactly where they
            // already are — blocked, with nothing else to press.
            <button type="button" className="btn" disabled>
              {t("settings.reinstallRunning")}
            </button>
          ) : installer.confirming ? (
            <>
              <button type="button" className="btn ghost" onClick={installer.cancel}>
                {t("settings.reinstallCancel")}
              </button>
              <button type="button" className="btn" onClick={installer.run}>
                {t("settings.reinstallGo")}
              </button>
            </>
          ) : (
            <button type="button" className="btn" onClick={installer.ask}>
              {t("upgrade.now")}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * Poll the daemon for an outstanding refusal.
 *
 * Slower than the profile poll next to it: this state changes when an operator
 * moves a platform-wide floor, not when the user does anything, and the dialog
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
        // A daemon that is down cannot be refused by anything; leave the dialog
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
