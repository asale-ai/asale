// The client's one always-visible status readout (top right, every page).
//
// It answers "is this client actually working right now" without the user
// having to open a page and read three cards. That question is never just "is
// the socket up": a device can be online and earning nothing because no account
// is switched on, every lane is cooling, or it was never signed in. Each of
// those is a distinct state here, with its own colour, and its own one-line
// "what to do about it" in the tooltip.
import { useEffect, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { invoke, type ClientStatus } from "../lib";
import { IconWifi } from "../icons";

type Tone = "ok" | "warn" | "bad" | "idle";
/** One resolved state: which colour, which text block, and whether the dot
 *  should breathe (reserved for "it is working on it, wait"). */
interface View { tone: Tone; key: string; pulse?: boolean }

/** Collapse the raw status into the single state the user is actually in.
 *  Order is severity, not sequence: a signed-out client's link state is noise. */
function resolve(down: boolean, s: ClientStatus | null): View {
  if (down || !s) return { tone: "bad", key: "daemonDown" };
  if (!s.signed_in) return { tone: "warn", key: "signedOut" };
  switch (s.publish_state) {
    case "online":
      // Online with accounts on but nothing sellable is its own state: the
      // link is fine, the earning is not, and only the lanes explain why.
      return s.selling.length > 0 && s.lanes_selling === 0
        ? { tone: "warn", key: "onlineIdle" }
        : { tone: "ok", key: "online" };
    case "throttled":
      return { tone: "warn", key: "throttled" };
    case "connecting":
      return { tone: "warn", key: "connecting", pulse: true };
    case "reconnecting":
      return { tone: "warn", key: "reconnecting", pulse: true };
    case "kicked":
      return { tone: "bad", key: "kicked" };
    default:
      // Offline. With accounts switched on this is transient (the daemon
      // brings the session up); with none it is the intended resting state.
      if (s.selling.length > 0) return { tone: "warn", key: "pending", pulse: true };
      return { tone: "idle", key: s.buying.length > 0 ? "buyOnly" : "idle" };
  }
}

/** `https://api.example.com/x` → `api.example.com`; falls back to the raw value. */
const host = (url: string) => {
  try { return new URL(url).host; } catch { return url; }
};

export function StatusWidget() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<ClientStatus | null>(null);
  const [down, setDown] = useState(false);
  // `pinned` = opened by click, so the details survive moving the pointer into
  // them (copying a device id, reading a long server URL).
  const [hovering, setHovering] = useState(false);
  const [pinned, setPinned] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    const poll = () =>
      invoke<ClientStatus>("client_status")
        .then((s) => { if (alive) { setStatus(s); setDown(false); } })
        .catch(() => { if (alive) setDown(true); });
    poll();
    const id = setInterval(poll, 5000);
    return () => { alive = false; clearInterval(id); };
  }, []);

  useEffect(() => {
    if (!pinned) return;
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setPinned(false);
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") setPinned(false); };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [pinned]);

  const v = resolve(down, status);
  const open = pinned || hovering;
  const s = status;

  const row = (label: string, value: ReactNode, tone?: Tone) => (
    <div className="sw-row">
      <span className="sw-k">{label}</span>
      <span className={`sw-v${tone ? ` tone-${tone}` : ""}`}>{value}</span>
    </div>
  );

  return (
    <div
      className="sw"
      ref={ref}
      onMouseEnter={() => setHovering(true)}
      onMouseLeave={() => setHovering(false)}
    >
      <button
        className={`sw-chip tone-${v.tone}${open ? " open" : ""}`}
        onClick={() => setPinned((p) => !p)}
        onFocus={() => setHovering(true)}
        onBlur={() => setHovering(false)}
        aria-expanded={open}
        type="button"
      >
        <span className={`sw-dot${v.pulse ? " pulse" : ""}`} />
        <span className="sw-label">{t(`status.${v.key}.label`)}</span>
      </button>

      {open && (
        <div className="sw-pop" role="status">
          <div className="sw-head">
            <span className={`sw-ico tone-${v.tone}`}><IconWifi /></span>
            <div>
              <div className="sw-title">{t(`status.${v.key}.label`)}</div>
              <div className="sw-desc">{t(`status.${v.key}.desc`)}</div>
            </div>
          </div>

          {/* Facts, ordered by how often they explain the state above. Dropped
              entirely once the daemon stops answering: the last poll's numbers
              are no longer true, and stale counts read as current ones. */}
          {!down && s && (
            <div className="sw-rows">
              {row(
                t("status.rowSell"),
                t("status.sellCount", { on: s.selling.length, total: s.accounts_total }),
                s.selling.length > 0 ? "ok" : "idle",
              )}
              {s.selling.length > 0 && row(
                t("status.rowLanes"),
                <>
                  {s.lanes_selling}
                  {s.lanes_blocked > 0 && (
                    <span className="sw-sub"> · {t("status.lanesBlocked", { n: s.lanes_blocked })}</span>
                  )}
                </>,
                s.lanes_selling > 0 ? "ok" : "warn",
              )}
              {row(
                t("status.rowBuy"),
                s.buying.length > 0 ? s.buying.join(", ") : t("status.buyNone"),
                s.buying.length > 0 ? "ok" : "idle",
              )}
              {row(t("status.rowServer"), <span className="mono">{host(s.server_api_base)}</span>)}
              {row(
                t("status.rowProxy"),
                <span className="mono">
                  127.0.0.1:{s.proxy_port}
                  {!s.api_key_loaded && <span className="sw-sub"> · {t("status.keyMissing")}</span>}
                </span>,
                s.api_key_loaded ? undefined : "warn",
              )}
              {row(
                t("status.rowDevice"),
                <span className="mono" title={s.device_id}>{s.device_id.slice(0, 12)} · v{s.version}</span>,
              )}
            </div>
          )}

          <div className="sw-hint">{t(`status.${v.key}.hint`)}</div>
        </div>
      )}
    </div>
  );
}
