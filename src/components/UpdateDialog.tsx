// The one place an update is confirmed and watched.
//
// Opened from the top-bar button and from Settings alike, so both routes look
// and behave the same: say what installing costs, show the release notes, then
// the progress bar while it downloads. The state behind it lives in
// `lib/updates` and is shared, so a dialog closed and reopened — or opened from
// the other place — picks up the download already in flight.

import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { IconExternal, IconX } from "../icons";
import { openExternal } from "../shell";
import { hasPendingUpdate, useInstaller, useUpdateState } from "../lib/updates";
import { UpdateProgress } from "./UpdateProgress";

export function UpdateDialog({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const update = useUpdateState();
  const installer = useInstaller();
  const available = hasPendingUpdate(update);

  const close = () => {
    // Mid-download this only hides the window: the download belongs to the
    // shared store, not to this component, so it keeps going and reopening the
    // dialog (from either place) shows it where it got to. Only an idle
    // confirmation is actually withdrawn.
    if (!installer.running) installer.cancel();
    onClose();
  };

  // Portaled: the top bar is a `backdrop-filter` layer, and a fixed backdrop
  // inside one is positioned against that strip rather than the window.
  return createPortal(
    <div className="modal-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) close(); }}>
      <div className="modal" role="dialog" aria-modal="true" aria-labelledby="update-title">
        <div className="modal-head">
          {/* Settings can open this with nothing new published — then it is a
              reinstall of the current build, and the title says so. */}
          <h3 id="update-title">
            {available ? t("settings.updateAvailable", { version: update.latest }) : t("settings.reinstallButton")}
          </h3>
          <button
            type="button"
            className="modal-x"
            onClick={close}
            title={installer.running ? t("common.close") : t("settings.reinstallCancel")}
          >
            <IconX />
          </button>
        </div>
        <div className="upgrade-body">
          <p className="text-sm">{t("settings.reinstallConfirm")}</p>
          {available && update.page && (
            <div className="btn-row">
              <button type="button" className="btn ghost" onClick={() => openExternal(update.page)}>
                <IconExternal />
                {t("settings.updateNotes")}
              </button>
            </div>
          )}
          <UpdateProgress installer={installer} />
          {installer.running && <p className="hint">{t("settings.updateBackgroundHint")}</p>}
          {installer.error && (
            <p className="text-sm">{t("settings.reinstallError", { msg: installer.error })}</p>
          )}
        </div>
        <div className="modal-foot">
          <div className="modal-spacer" />
          {installer.running ? (
            <>
              <button type="button" className="btn ghost" onClick={close}>
                {t("common.close")}
              </button>
              <button type="button" className="btn" disabled>{t("settings.reinstallRunning")}</button>
            </>
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
  );
}
