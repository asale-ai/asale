import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri,
  type UsageLimits, type LimitProvider, type LimitWindow,
} from "../lib";
import { computePace, resetToMs, formatReset, formatExactReset, type DisplayMode, type Pace } from "../lib/limit-pace";
import { PageHead, IconAction, Mark } from "../ui";
import { IconRefresh, IconPlus } from "../icons";

const PROVIDERS = [
  { id: "claude", label: "Claude Code" },
  { id: "claude_work", label: "Claude Work" },
  { id: "codex", label: "Codex" },
  { id: "gemini", label: "Gemini" },
  { id: "kimi", label: "Kimi Code" },
  { id: "kimi_api", label: "Moonshot API" },
  { id: "xai", label: "Grok CLI" },
  { id: "xai_api", label: "xAI API" },
];
/** Exact id first: several ids are prefixes of others (`kimi`/`kimi_api`,
 *  `claude`/`claude_work`), and a prefix match alone labels the platform-API
 *  rows with the subscription's name. */
const meta = (id: string) =>
  PROVIDERS.find((p) => p.id === id) ?? PROVIDERS.find((p) => id.startsWith(p.id)) ?? { label: id };

/** Threshold color; in "remaining" mode a low value is bad (mirrored). */
function barTone(displayPct: number, mode: DisplayMode): "ok" | "warn" | "danger" {
  const pct = mode === "remaining" ? 100 - displayPct : displayPct;
  if (pct >= 90) return "danger";
  if (pct >= 70) return "warn";
  return "ok";
}

function formatPercentValue(value: number, subOne: string): string {
  const pct = Math.max(0, Math.min(100, Number(value) || 0));
  const rounded = Math.round(pct);
  if (pct > 0 && rounded === 0) return subOne;
  return String(rounded);
}

/** One quota-window bar: label · track (fill + pace notch) · % · reset. */
function LimitBar({
  label, pct, reset, mode, pace, title,
}: {
  label: string; pct: number; reset: string | null; mode: DisplayMode; pace: Pace; title?: string;
}) {
  const { t } = useTranslation();
  const rawUsed = Math.max(0, Math.min(100, Number(pct) || 0));
  const displayPct = mode === "remaining" ? 100 - rawUsed : rawUsed;
  const rounded = Math.round(displayPct);
  const widthPct = displayPct > 0 && rounded === 0 ? Math.max(displayPct, 0.35) : displayPct;
  const labelPct = displayPct > 0 && rounded === 0 ? t("limits.subOnePercent") : String(rounded);
  const paceX = pace.pacePercent == null ? null : Math.max(0, Math.min(100, pace.pacePercent));
  return (
    <div className="limit-row" title={title}>
      <span className="lr-label" data-limit-label="">{label}</span>
      <div className="limit-track">
        <div className={`limit-fill ${barTone(displayPct, mode)}`} style={{ width: `${widthPct}%`, minWidth: displayPct > 0 ? 3 : 0 }} />
        {paceX != null && (
          <>
            <div className="limit-notch-cut" style={{ left: `calc(${paceX}% - 3px)` }} />
            <div className={`limit-notch-tick ${pace.paceOver ? "over" : "under"}`} style={{ left: `calc(${paceX}% - 1px)` }} />
          </>
        )}
      </div>
      <span className="lr-pct">{labelPct}%</span>
      {reset && <span className="lr-reset">{reset}</span>}
    </div>
  );
}

/** One provider = one card. A connected provider shows its windows; a
 *  disconnected one still gets a card, because "Gemini is not connected" is an
 *  answer the page owes the user, not a row to hide. */
function ToolGroup({
  id, name, badge, children, connected = true, expandable = false, expanded = false, onToggle,
}: {
  id: string; name: string; badge?: React.ReactNode; children: React.ReactNode;
  connected?: boolean; expandable?: boolean; expanded?: boolean; onToggle?: () => void;
}) {
  const header = (
    <div className="limit-group-head">
      <Mark id={id} size="sm" />
      <span className="lg-name">{name}</span>
      {badge}
    </div>
  );
  const cls = `limit-group${connected ? "" : " disconnected"}`;
  if (!expandable) return <div className={cls}>{header}{children}</div>;
  return (
    <div
      className={`${cls} expandable ${expanded ? "expanded" : ""}`}
      role="button" tabIndex={0} aria-expanded={expanded}
      onClick={onToggle}
      onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); onToggle?.(); } }}
    >
      {header}{children}
    </div>
  );
}

export function Limits() {
  const { t, i18n } = useTranslation();
  const [data, setData] = useState<UsageLimits | null>(null);
  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const [labelWidth, setLabelWidth] = useState(0);
  // Display is fixed to "used" — this page only surfaces consumed quota.
  const mode = "used" as DisplayMode;

  const load = useCallback((force = false) => {
    if (!inTauri) {
      // Browser preview: no backend to call, but still surface the refresh
      // loading affordance so the button visibly spins on click.
      if (force) { setRefreshing(true); setTimeout(() => setRefreshing(false), 900); }
      return;
    }
    if (force) setRefreshing(true);
    invoke<UsageLimits>("usage_limits", { force })
      .then(setData)
      .finally(() => setRefreshing(false));
  }, []);

  useEffect(() => {
    load();
    // Poll on a slow cadence — the live Claude fetch is server-cached ~60s, so
    // this mostly serves cached windows; manual refresh forces a fresh fetch.
    const id = setInterval(() => load(), 30000);
    return () => clearInterval(id);
  }, [load]);

  // Measure the widest window label so every bar's label column aligns.
  useLayoutEffect(() => {
    const root = containerRef.current;
    if (!root) return;
    const labels = root.querySelectorAll<HTMLElement>("[data-limit-label]");
    if (labels.length === 0) { setLabelWidth((p) => (p === 0 ? p : 0)); return; }
    let max = 0;
    const ctx = document.createElement("canvas").getContext("2d");
    if (ctx) {
      const s = window.getComputedStyle(labels[0]);
      ctx.font = `${s.fontStyle} ${s.fontWeight} ${s.fontSize} ${s.fontFamily}`;
      labels.forEach((el) => { max = Math.max(max, ctx.measureText(el.textContent || "").width); });
    }
    const next = Math.ceil(max);
    setLabelWidth((prev) => (prev === next ? prev : next));
  });

  // Pace + explanation for a window, in the active display mode.
  const paceFor = (w: LimitWindow): Pace =>
    computePace({ usedPercent: w.used_percent, windowSeconds: w.window_seconds, resetMs: resetToMs(w.reset_at), mode });

  const hoverFor = (w: LimitWindow, pace: Pace): string => {
    const lines = [explainLine(w, pace)];
    const exact = formatExactReset(w.reset_at, i18n.language);
    if (exact) lines.push(t("limits.resetsAt", { time: exact }));
    return lines.join("\n");
  };

  const explainLine = (w: LimitWindow, pace: Pace): string => {
    const remaining = mode === "remaining";
    const project = (u: number) => (remaining ? 100 - u : u);
    const sub = t("limits.subOnePercent");
    if (pace.expectedPercent == null) {
      const used = formatPercentValue(project(w.used_percent), sub);
      return t(remaining ? "limits.explainRemaining" : "limits.explainUsed", { label: w.label, used });
    }
    if (pace.paceOver) {
      if (pace.runsOutEta) return t("limits.explainAheadEta", { label: w.label, eta: pace.runsOutEta });
      const pct = formatPercentValue(project(pace.projectedEnd ?? 100), sub);
      return t(remaining ? "limits.explainAheadPctRemaining" : "limits.explainAheadPct", { label: w.label, pct });
    }
    const pct = formatPercentValue(project(pace.projectedEnd ?? w.used_percent), sub);
    return t(remaining ? "limits.explainOnTrackRemaining" : "limits.explainOnTrack", { label: w.label, pct });
  };

  const renderProvider = (p: LimitProvider) => {
    const m = meta(p.id);
    if (!p.connected) {
      return (
        <ToolGroup key={p.id} id={p.id} name={m.label} connected={false}>
          <div className="limit-status">
            <span>{t("limits.notConnected")}</span>
            <button
              className="lane-resume"
              onClick={() => window.dispatchEvent(new CustomEvent("asale:nav", { detail: "publish" }))}
            >
              <IconPlus /> {t("limits.connect")}
            </button>
          </div>
        </ToolGroup>
      );
    }
    const name = p.plan_label ? `${m.label} ${p.plan_label}` : m.label;
    const rows = (p.windows || []).map((w) => ({ w, pace: paceFor(w) }));
    const expanded = expandedId === p.id;
    // An estimate has to say so. Silently showing served-vs-cap numbers in the
    // same bars as the provider's own reads makes a failed fetch look like real
    // data — and hides the windows the provider reports that we cannot derive
    // (the weekly per-model ones).
    const reason = p.fallback_reason || "";
    const badge = p.live
      ? <span className="pill on tiny">{t("limits.live")}</span>
      : (
        <span
          className="pill warn tiny"
          title={reason ? t("limits.estimateFailed") : t("limits.estimateUnsupported")}
        >
          {t("limits.estimate")}
        </span>
      );
    return (
      <ToolGroup
        key={p.id} id={p.id} name={name} badge={badge}
        expandable={rows.length > 0} expanded={expanded}
        onToggle={() => setExpandedId((prev) => (prev === p.id ? null : p.id))}
      >
        {rows.map(({ w, pace }) => (
          <LimitBar
            key={w.key} label={w.label} pct={w.used_percent}
            reset={formatReset(w.reset_at, t("limits.now"))}
            mode={mode} pace={pace} title={hoverFor(w, pace)}
          />
        ))}
        {reason && (
          <div className="limit-fallback" title={reason}>
            <span>{t("limits.estimateFailed")}</span>
            <button
              className="lane-resume"
              onClick={(e) => { e.stopPropagation(); window.dispatchEvent(new CustomEvent("asale:nav", { detail: "settings" })); }}
            >
              {t("limits.fixProxy")}
            </button>
          </div>
        )}
        {expanded && (
          <div className="limit-detail">
            {rows.map(({ w, pace }) => {
              const exact = formatExactReset(w.reset_at, i18n.language);
              return (
                <div key={w.key} className="ld-line">
                  {explainLine(w, pace)}
                  {exact && <span className="ld-when">{" · "}{t("limits.resetsAt", { time: exact })}</span>}
                </div>
              );
            })}
            {reason && <div className="ld-line">{t("limits.estimateReason", { reason })}</div>}
          </div>
        )}
      </ToolGroup>
    );
  };

  const providers = data?.providers ?? PROVIDERS.map((p) => ({ id: p.id, connected: false }));

  const connectedCount = providers.filter((p) => p.connected).length;

  return (
    <div>
      <PageHead
        title={t("limits.title")}
        sub={t("limits.sub")}
        actions={
          <IconAction
            icon={<IconRefresh />}
            label={t("limits.refresh")}
            onClick={() => load(true)}
            disabled={!inTauri || refreshing}
            spinning={refreshing}
          />
        }
      />

      <div
        className="limits-panel"
        ref={containerRef}
        style={labelWidth > 0 ? ({ "--tt-limits-label-w": `${labelWidth}px` } as React.CSSProperties) : undefined}
      >
        <div className="limits-panel-head">
          <h3 className="limits-panel-title">{t("limits.panelTitle")}</h3>
          <span className="count-chip">{connectedCount}/{providers.length}</span>
        </div>
        <div className="limit-cards">{providers.map(renderProvider)}</div>
        <p className="limits-foot">{t("limits.explainBody")}</p>
      </div>
    </div>
  );
}
