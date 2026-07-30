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
//
// The window sizes itself to this content (`shell.resizePanel`, measured below)
// rather than being a fixed rectangle the layout has to fill: what the panel
// shows changes with the state — no rows at all when the daemon is down, one
// extra when accounts are selling — and any fixed height is wrong for most of
// them, in the visible way a menu with a hole in it is wrong.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke as rpc, fmtUsdt, type ClientStatus, type UsageSummary, type UsageBucket } from "../lib";
import { resolveStatus } from "./StatusWidget";
import { IconExternal, IconGlobe, IconPower, IconLoader, IconWifi } from "../icons";
import { shell } from "../shell";

export function TrayPanel() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<ClientStatus | null>(null);
  const [down, setDown] = useState(false);
  const [ready, setReady] = useState(false);
  const [sold, setSold] = useState<UsageBucket | null>(null);
  const [busy, setBusy] = useState<"" | "desktop" | "web" | "quit">("");
  const box = useRef<HTMLDivElement>(null);

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
        .then((s) => { if (alive) setSold(s.sold); })
        .catch(() => {});
    load();
    const id = setInterval(load, 30_000);
    return () => { alive = false; clearInterval(id); };
  }, []);

  // Keep the window exactly as tall as what is in it. Measured from the panel
  // box rather than the document because the document is what the window sizes,
  // and reading the thing you are about to change is how a resize loop starts.
  useEffect(() => {
    const el = box.current;
    if (!el || !shell.available) return;
    let last = 0;
    const push = () => {
      // Ceil: a fractional layout height rounded down clips the last line by a
      // pixel, which on the footer reads as a rendering fault.
      const h = Math.ceil(el.getBoundingClientRect().height);
      if (h > 0 && h !== last) {
        last = h;
        void shell.resizePanel(h);
      }
    };
    const ro = new ResizeObserver(push);
    ro.observe(el);
    push();
    // Also on every open. The window is built hidden at startup and never torn
    // down, so the first measurement is taken on a window that has never been
    // on screen — a platform that lays that out as nothing would otherwise leave
    // the panel at its starting height until something in it changed.
    const again = () => { last = 0; push(); };
    window.addEventListener("focus", again);
    return () => {
      ro.disconnect();
      window.removeEventListener("focus", again);
    };
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
    <div className="tp" ref={box}>
      <div className="tp-head">
        <span className={`tp-ico tone-${v.tone}${v.pulse ? " pulse" : ""}`}>
          <IconWifi />
        </span>
        <div className="tp-head-text">
          <div className="tp-title">{t(`status.${v.key}.label`)}</div>
          <div className="tp-sub">{t(`status.${v.key}.desc`)}</div>
        </div>
      </div>

      {/* The one figure this window exists to show. It is a tile rather than a
          fourth key/value row because on a machine that is only ever selling it
          is the answer, and the rows below are the explanation. */}
      {!down && (
        <div className="tp-earn">
          <div className="tp-earn-top">
            <span className="tp-earn-k">{t("tray.earnedToday")}</span>
            {sold !== null && sold.count > 0 && (
              <span className="tp-earn-meta">{t("tray.calls", { n: sold.count })}</span>
            )}
          </div>
          {/* Null, not zero, until the figure has been read: a hard 0.00 on a
              device that earned all morning is a worse answer than a dash. */}
          {sold === null ? (
            <div className="tp-earn-v idle">—</div>
          ) : (
            <div className="tp-earn-v">
              <span className="mono">{fmtUsdt(sold.amount_usdt)}</span>
              <span className="tp-earn-u">USDT</span>
            </div>
          )}
        </div>
      )}

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
        </div>
      )}

      {/* What to do about it — only when there is something to do. On a healthy
          client this would be a line of reassurance nobody reads. */}
      {v.tone !== "ok" && <div className="tp-hint">{t(`status.${v.key}.hint`)}</div>}

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

      {/* Quitting really quits: on a selling machine that is the difference
          between "I closed the window" and "I stopped earning". */}
      <div className="tp-foot">
        <button className="tp-quit" disabled={busy !== ""} onClick={() => act("quit", () => shell.quit())}>
          <IconPower />
          {t("tray.quit")}
        </button>
        <span className="tp-foot-note">{t("tray.quitNote")}</span>
      </div>
    </div>
  );
}
