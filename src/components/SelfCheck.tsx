// Environment faults the app can detect, and — where it can — repair.
//
// The button performs the whole fix rather than opening the page the setting
// lives on. Navigating there was the version of this that did not work: the
// user who cannot tell a regional 403 from a login problem also cannot tell
// which port the proxy on their own machine listens on, so the Settings page
// they arrive at asks them the one question they came here unable to answer.
//
// Nothing runs unasked. The two repairs that exist spend a subscription's quota
// (selling) or route the user's provider traffic somewhere new (the proxy), and
// a click is what makes that theirs. Faults whose repair lives outside the app —
// an environment variable, how the app is launched — come back `fixable: false`
// and stay a sentence, because a button that only navigates is the thing this
// replaced.

import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, inTauri } from "../lib";
import { errText } from "../errors";
import { IconAlert, IconInfo } from "../icons";

interface Finding {
  id: string;
  severity: "error" | "warn" | "info";
  params: Record<string, unknown>;
  fixable?: boolean;
}

/** What the daemon did, in the same key+params shape every command speaks. */
interface FixResult {
  fixed: boolean;
  key: string;
  params?: Record<string, unknown>;
}

/** Per-finding state of the repair button, so two findings never share a spinner. */
type Outcome = { busy: boolean; text?: string; ok?: boolean };

export function SelfCheck() {
  const { t } = useTranslation();
  const [findings, setFindings] = useState<Finding[]>([]);
  const [outcomes, setOutcomes] = useState<Record<string, Outcome>>({});

  const alive = useRef(true);
  const scan = useCallback(() => {
    invoke<{ findings: Finding[] }>("self_check")
      .then((r) => {
        if (alive.current) setFindings(r.findings ?? []);
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    alive.current = true;
    // Once on mount, and again after a repair. These are environment faults — a
    // mis-set variable, a switch left off — not live state, so polling them
    // would burn round trips to re-read an answer that only changes when
    // something acts on it.
    if (inTauri) scan();
    return () => {
      alive.current = false;
    };
  }, [scan]);

  async function fix(id: string) {
    setOutcomes((o) => ({ ...o, [id]: { busy: true } }));
    try {
      const r = await invoke<FixResult>("selfcheck_fix", { id });
      setOutcomes((o) => ({
        ...o,
        [id]: { busy: false, ok: r.fixed, text: t(r.key, r.params as Record<string, string>) },
      }));
      // Re-scan only on success: a repair that worked should make the callout
      // disappear, and one that did not must leave it on screen next to the
      // reason. Both are the same call, so the difference has to be here.
      if (r.fixed) scan();
    } catch (e) {
      setOutcomes((o) => ({ ...o, [id]: { busy: false, ok: false, text: errText(e) } }));
    }
  }

  if (findings.length === 0) return null;

  return (
    <>
      {findings.map((f) => {
        const out = outcomes[f.id];
        return (
          <div key={f.id} className={`callout ${f.severity === "error" ? "danger" : "warn"} card-lead`}>
            {f.severity === "error" ? <IconAlert /> : <IconInfo />}
            <div style={{ flex: 1 }}>
              <strong>{t(`selfcheck.${f.id}.title`)}</strong>
              <div className="text-sm" style={{ marginTop: 4 }}>
                {t(`selfcheck.${f.id}.body`, f.params as Record<string, string>)}
              </div>
              {out?.text && (
                <div className="text-sm" style={{ marginTop: 6, opacity: out.ok ? 1 : 0.85 }}>
                  {out.ok ? "✓ " : "— "}
                  {out.text}
                </div>
              )}
            </div>
            {f.fixable && (
              <button type="button" className="btn btn-sm" disabled={out?.busy} onClick={() => fix(f.id)}>
                {out?.busy ? t("selfcheck.fixing") : t(`selfcheck.${f.id}.fix`)}
              </button>
            )}
          </div>
        );
      })}
    </>
  );
}
