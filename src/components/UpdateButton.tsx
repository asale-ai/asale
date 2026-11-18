// "There is a new version" — in the top bar, where it can be acted on.
//
// The sidebar already carries a dot on the Settings item, but a dot is a
// direction rather than an action: the update card is five cards down that
// page, so the shortest route to installing was "notice a dot, open Settings,
// scroll, read, press". This is the same news with the button attached.
//
// It opens the same dialog Settings opens (`UpdateDialog`), from
// the same shared state in `lib/updates` — an update started here is watched
// from either of the others, and vice versa. Nothing new is downloaded or
// installed by this file; it only asks earlier.
//
// It still asks. Closing the app and prompting for an administrator password
// is not something a single click in a top bar should be able to do by
// surprise, so the button opens the confirmation rather than the installer.

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { IconDownload } from "../icons";
import { hasPendingUpdate, useInstaller, useUpdateState } from "../lib/updates";
import { UpdateDialog } from "./UpdateDialog";

export function UpdateButton() {
  const { t } = useTranslation();
  const update = useUpdateState();
  const installer = useInstaller();
  const [open, setOpen] = useState(false);

  // Only when there is something to install. Outside the desktop shell the
  // check never runs, so this is also what keeps the button out of a browser
  // pointed at a remote daemon — it cannot install onto that machine.
  if (!hasPendingUpdate(update)) return null;

  return (
    <>
      <button
        type="button"
        className="pill warn act"
        onClick={() => { setOpen(true); if (!installer.running) installer.ask(); }}
      >
        <IconDownload />
        {t("update.button", { version: update.latest })}
      </button>
      {open && <UpdateDialog onClose={() => setOpen(false)} />}
    </>
  );
}
