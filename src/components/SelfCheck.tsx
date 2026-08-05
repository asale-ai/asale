// Environment faults the app can detect but not safely repair on its own.
//
// Each finding is a sentence plus the one action that resolves it. Nothing here
// changes anything by itself: the fixes involve moving credentials or starting
// to spend a subscription's quota, and doing either without being asked is worse
// than the fault being reported.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, inTauri } from "../lib";
import { IconAlert, IconInfo } from "../icons";

interface Finding {
  id: string;
  severity: "error" | "warn" | "info";
  params: Record<string, unknown>;
}

/** Where the finding's "fix this" button sends the user, if anywhere. */
const GOES_TO: Record<string, "publish" | "settings" | undefined> = {
  sellAllOff: "publish",
  regionBlocked: "settings",
};

export function SelfCheck() {
  const { t } = useTranslation();
  const [findings, setFindings] = useState<Finding[]>([]);

  useEffect(() => {
    if (!inTauri) return;
    let alive = true;
    // Once per mount. These are environment faults — a mis-set variable, a
    // switch left off — not live state, so polling them would burn round trips
    // to re-read an answer that changes when the user acts, and acting means
    // leaving this page anyway.
    invoke<{ findings: Finding[] }>("self_check")
      .then((r) => {
        if (alive) setFindings(r.findings ?? []);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, []);

  if (findings.length === 0) return null;

  return (
    <>
      {findings.map((f) => {
        const to = GOES_TO[f.id];
        return (
          <div key={f.id} className={`callout ${f.severity === "error" ? "danger" : "warn"} card-lead`}>
            {f.severity === "error" ? <IconAlert /> : <IconInfo />}
            <div style={{ flex: 1 }}>
              <strong>{t(`selfcheck.${f.id}.title`)}</strong>
              <div className="text-sm" style={{ marginTop: 4 }}>
                {t(`selfcheck.${f.id}.body`, f.params as Record<string, string>)}
              </div>
            </div>
            {to && (
              <button
                type="button"
                className="btn btn-sm"
                onClick={() => window.dispatchEvent(new CustomEvent("asale:nav", { detail: to }))}
              >
                {t(`selfcheck.${f.id}.action`)}
              </button>
            )}
          </div>
        );
      })}
    </>
  );
}
