// "There is a new version" — in the top bar, where it can be acted on.
//
// The sidebar already carries a dot on the Settings item, but a dot is a
// direction rather than an action: the update card is five cards down that
// page, so the shortest route to installing was "notice a dot, open Settings,
// scroll, read, press". This is the same news with the button attached.
//
// It runs the same sequence Settings and the forced-upgrade dialog run, from
// the same shared state in `lib/updates` — an update started here is watched
// from either of the others, and vice versa. Nothing new is downloaded or
// installed by this file; it only asks earlier.
//
// It still asks. Closing the app and prompting for an administrator password
// is not something a single click in a top bar should be able to do by
// surprise, so the button opens the confirmation rather than the installer.

import { useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { IconDownload, IconExternal, IconX } from "../icons";
import { openExternal } from "../shell";
import { hasPendingUpdate, useInstaller, useUpdateState } from "../lib/updates";
import { UpdateProgress } from "./UpdateProgress";

export function UpdateButton() {
  const { t } = useTranslation();
  const update = useUpdateState();
  const installer = useInstaller();
  const [open, setOpen] = useState(false);

  // Only when there is something to install. Outside the desktop shell the
  // check never runs, so this is also what keeps the button out of a browser
  // pointed at a remote daemon — it cannot install onto that machine.
  if (!hasPendingUpdate(update)) return null;

  const close = () => {
    setOpen(false);
    installer.cancel();
  };

  return (
    <>
      <button
        type="button"
        className="pill warn act"
        onClick={() => { setOpen(true); installer.ask(); }}
      >
        <IconDownload />
        {t("update.button", { version: update.latest })}
      </button>

      {/* Portaled: the top bar is a `backdrop-filter` layer, and a fixed
          backdrop inside one is positioned against that strip rather than the
          window — the dialog came up clipped by the 40px it is drawn in. */}
      {open && createPortal(
        // Dismissable, unlike the forced-upgrade gate: nothing is blocked yet,
        // and "not now" is a legitimate answer to an optional update.
        <div className="modal-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget && !installer.running) close(); }}>
          <div className="modal" role="dialog" aria-modal="true" aria-labelledby="update-title">
            <div className="modal-head">
              <h3 id="update-title">{t("settings.updateAvailable", { version: update.latest })}</h3>
              {!installer.running && (
                <button type="button" className="modal-x" onClick={close} title={t("settings.reinstallCancel")}>
                  <IconX />
                </button>
              )}
            </div>
            <div className="upgrade-body">
              <p className="text-sm">{t("settings.reinstallConfirm")}</p>
              {update.page && (
                <div className="btn-row">
                  <button type="button" className="btn ghost" onClick={() => openExternal(update.page)}>
                    <IconExternal />
                    {t("settings.updateNotes")}
                  </button>
                </div>
              )}
              <UpdateProgress installer={installer} />
              {installer.error && (
                <p className="text-sm">{t("settings.reinstallError", { msg: installer.error })}</p>
              )}
            </div>
            <div className="modal-foot">
              <div className="modal-spacer" />
              {installer.running ? (
                <button type="button" className="btn" disabled>{t("settings.reinstallRunning")}</button>
              ) : (
                <>
                  <button type="button" className="btn ghost" onClick={close}>
                    {t("settings.reinstallCancel")}
                  </button>
                  <button type="button" className="btn" onClick={installer.run}>
                    {t("settings.reinstallGo")}
                  </button>
                </>
              )}
            </div>
          </div>
        </div>,
        document.body,
      )}
    </>
  );
}
