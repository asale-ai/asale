// The bar the user watches while the release downloads.
//
// One component, rendered by both places an update can be started from — the
// forced-upgrade dialog and the Settings card. They are two different framings
// of the same operation, and a user who starts it in one and looks at the other
// should not have to work out whether they are seeing the same download.
//
// Indeterminate is a real state and it is drawn as one: the manifest usually
// carries sizes, but when it does not, a bar filled to a made-up percentage is
// worse than a byte count that is simply true.

import { useTranslation } from "react-i18next";
import type { Installer } from "../lib/updates";

/** MB with one decimal — the unit every release asset is measured in. */
function mb(bytes: number): string {
  return (bytes / 1024 / 1024).toFixed(1);
}

export function UpdateProgress({ installer }: { installer: Installer }) {
  const { t } = useTranslation();

  if (installer.phase === "installing") {
    // Nothing left to measure: the app is about to close, the installer takes
    // over, and the next thing the user sees is the password prompt.
    return <div className="text-sm">{t("update.installing")}</div>;
  }
  if (installer.phase !== "downloading") return null;

  const pct = installer.progress === null ? null : Math.round(installer.progress * 100);
  return (
    <div className="update-progress">
      <div className="update-progress-head">
        <span>{t("update.downloading")}</span>
        <b>
          {installer.total > 0
            ? t("update.downloadedOf", { done: mb(installer.received), total: mb(installer.total) })
            : t("update.downloaded", { done: mb(installer.received) })}
        </b>
      </div>
      <div className={`bar${pct === null ? " indeterminate" : ""}`}>
        <span style={pct === null ? undefined : { width: `${pct}%` }} />
      </div>
    </div>
  );
}
