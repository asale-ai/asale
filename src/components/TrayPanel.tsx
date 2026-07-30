// The tray overview panel — a small always-on-top window the tray icon opens.
//
// It exists because closing the main window hides asale to the tray, so on a
// machine that is only ever selling, the tray *is* the app for days at a time.
// A menu of text rows can say "online"; this can say online, with what, earning
// how much, and give the three things someone actually wants from there: the
// window, a browser, and a real quit.
//
// Rendered by main.tsx when the window URL carries `?view=panel`. Same bundle,
// same RPC client, same status resolution as the widget in the app (see
// `resolveStatus`) — a second implementation is a second thing to be wrong.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke as rpc, fmtUsdt, type ClientStatus, type UsageSummary } from "../lib";
import { resolveStatus } from "./StatusWidget";
import { IconExternal, IconGlobe, IconPower, IconLoader } from "../icons";
import { shell } from "../shell";

export function TrayPanel() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<ClientStatus | null>(null);
  const [down, setDown] = useState(false);
  const [ready, setReady] = useState(false);
  const [earned, setEarned] = useState<number | null>(null);
  const [busy, setBusy] = useState<"" | "desktop" | "web" | "quit">("");

  useEffect(() => {
    let alive = true;
    let fails = 0;
    const poll = () =>
      rpc<ClientStatus>("client_status")
        .then((s) => {
          if (!alive) return;
          fails = 0;
          setStatus(s);
          setDown(false);
          setReady(true);
        })
        .catch(() => {
          if (!alive) return;
          if (++fails >= 3) {
            setDown(true);
            setReady(true);
          }
        });
    poll();
    // Slower than the in-app widget: this window is only ever on screen for a
    // few seconds at a time, and it is open on machines that are otherwise idle.
    const id = setInterval(poll, 3000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  useEffect(() => {
    let alive = true;
    const load = () =>
      rpc<UsageSummary>("usage_summary", { period: "day" })
        .then((s) => { if (alive) setEarned(s.sold.amount_usdt); })
        .catch(() => {});
    load();
    const id = setInterval(load, 30_000);
    return () => { alive = false; clearInterval(id); };
  }, []);

  const v = resolveStatus(ready, down, status);
  const s = status;

  const row = (label: string, value: React.ReactNode, tone?: string) => (
    <div className="tp-row">
      <span className="tp-k">{label}</span>
      <span className={`tp-v${tone ? ` tone-${tone}` : ""}`}>{value}</span>
    </div>
  );

  // Every action closes the panel: it behaves like a menu, and a menu that
  // stays open after you pick something reads as a failed click.
  const act = async (which: "desktop" | "web" | "quit", run: () => Promise<unknown>) => {
    setBusy(which);
    try {
      await run();
    } finally {
      setBusy("");
    }
  };

  return (
    <div className="tp">
      <div className="tp-head">
        <span className={`tp-dot tone-${v.tone}${v.pulse ? " pulse" : ""}`} />
        <div className="tp-head-text">
          <div className="tp-title">{t(`status.${v.key}.label`)}</div>
          <div className="tp-sub">{t(`status.${v.key}.desc`)}</div>
        </div>
      </div>

      {!down && s && (
        <div className="tp-rows">
          {row(
            t("status.rowSell"),
            t("status.sellCount", { on: s.selling.length, total: s.accounts_total }),
            s.selling.length > 0 ? "ok" : "idle",
          )}
          {s.selling.length > 0 &&
            row(
              t("status.rowLanes"),
              <>
                {s.lanes_selling}
                {s.lanes_blocked > 0 && (
                  <span className="tp-note"> · {t("status.lanesBlocked", { n: s.lanes_blocked })}</span>
                )}
              </>,
              s.lanes_selling > 0 ? "ok" : "warn",
            )}
          {row(
            t("status.rowBuy"),
            s.buying.length > 0 ? s.buying.join(", ") : t("status.buyNone"),
            s.buying.length > 0 ? "ok" : "idle",
          )}
          {/* Null, not zero, until the figure has been read: a hard 0.00 on a
              device that earned all morning is a worse answer than a dash. */}
          {row(
            t("tray.earnedToday"),
            earned === null ? "—" : <span className="mono">{fmtUsdt(earned)} USDT</span>,
          )}
        </div>
      )}

      <div className="tp-actions">
        <button
          className="btn"
          disabled={busy !== ""}
          onClick={() => act("desktop", () => shell.showMainWindow())}
        >
          <IconExternal />
          {t("tray.openDesktop")}
        </button>
        <button
          className="btn ghost"
          disabled={busy !== ""}
          onClick={() => act("web", () => shell.openWebUi())}
        >
          {busy === "web" ? <IconLoader className="spin" /> : <IconGlobe />}
          {t("tray.openWeb")}
        </button>
      </div>

      <button className="tp-quit" disabled={busy !== ""} onClick={() => act("quit", () => shell.quit())}>
        <IconPower />
        {t("tray.quit")}
      </button>

      {/* Quitting really quits: on a selling machine that is the difference
          between "I closed the window" and "I stopped earning". */}
      <div className="tp-foot">{t("tray.quitNote")}</div>
    </div>
  );
}
