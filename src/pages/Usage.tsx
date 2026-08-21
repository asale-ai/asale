import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri, fmtTokens, fmtUsdt,
  type UsageOverview as Overview, type UsageScope, type UsagePeriod,
  type UsageDailyRow, type UsageHeatCell, type UsageModelRow,
} from "../lib";
import { Card, Skeleton, PageHead, IconAction, Err } from "../ui";
import { IconUsage, IconRefresh, IconInfo } from "../icons";
import { errText } from "../errors";

const PERIODS: UsagePeriod[] = ["day", "week", "month", "total"];
const SCOPES: UsageScope[] = ["used", "bought", "sold"];

// Model → provider family, for grouping the cards.
const FAMILY = (model: string): "claude" | "codex" | "gemini" | "other" => {
  const m = model.toLowerCase();
  if (m.includes("claude")) return "claude";
  if (m.startsWith("gpt") || m.includes("codex") || m.startsWith("o1") || m.startsWith("o3")) return "codex";
  if (m.includes("gemini")) return "gemini";
  return "other";
};

/** One hue, six steps. A distribution is ordered data, not four unrelated
 *  brands: ranking it by weight of the same accent reads instantly and keeps
 *  the page to a single colour. */
const rampStep = (idx: number) => Math.max(18, 100 - idx * 17);
const rampColor = (idx: number) =>
  `color-mix(in srgb, var(--accent) ${rampStep(idx)}%, var(--surface-3))`;

/** Ease-out count-up between value changes. */
function useCountUp(target: number, ms = 650): number {
  const [val, setVal] = useState(target);
  const prev = useRef(target);
  useEffect(() => {
    const from = prev.current;
    const to = target;
    if (from === to) return;
    const start = performance.now();
    let raf = 0;
    const tick = (t: number) => {
      const p = Math.min(1, (t - start) / ms);
      const eased = 1 - Math.pow(1 - p, 3);
      setVal(from + (to - from) * eased);
      if (p < 1) raf = requestAnimationFrame(tick);
      else { prev.current = to; setVal(to); }
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [target, ms]);
  return val;
}

export function Usage() {
  const { t, i18n } = useTranslation();
  const [period, setPeriod] = useState<UsagePeriod>("month");
  const [scope, setScope] = useState<UsageScope>("used");
  const [data, setData] = useState<Overview | null>(null);
  const [loading, setLoading] = useState(inTauri);
  const [refreshing, setRefreshing] = useState(false);
  const [fullFmt, setFullFmt] = useState(false);
  const [expandProv, setExpandProv] = useState<string | null>(null);
  const [err, setErr] = useState("");

  const load = useCallback((p: UsagePeriod, s: UsageScope, silent = false) => {
    if (!inTauri) {
      setLoading(false);
      // Browser preview: no backend, but still show the refresh spin on click.
      if (!silent) { setRefreshing(true); setTimeout(() => setRefreshing(false), 900); }
      return;
    }
    if (!silent) setRefreshing(true);
    invoke<Overview>("usage_overview", { period: p, scope: s })
      .then((d) => { setData(d); setErr(""); })
      // "我买的" is served by the asale server — it is the only party that knows
      // what a call cost — so signed out or offline it genuinely has no answer.
      // Swallowing that left the previous scope's numbers on screen under the
      // new label, which reads as data rather than as a failure.
      .catch((e) => { setData(null); setErr(errText(e)); })
      .finally(() => { setLoading(false); setRefreshing(false); });
  }, []);

  useEffect(() => { load(period, scope, true); }, [period, scope, load]);

  const total = data?.total_tokens ?? 0;
  const animated = useCountUp(total);
  const models = data?.models ?? [];
  const daily = data?.daily ?? [];
  const stats = data?.stats;

  // Provider-family rollup for the cards row.
  const families = useMemo(() => {
    const map = new Map<string, { tokens: number; count: number }>();
    for (const m of models) {
      const f = FAMILY(m.model);
      const e = map.get(f) ?? { tokens: 0, count: 0 };
      e.tokens += m.tokens; e.count += 1; map.set(f, e);
    }
    return [...map.entries()]
      .map(([id, v]) => ({ id, ...v, share: total > 0 ? (v.tokens / total) * 100 : 0 }))
      .sort((a, b) => b.tokens - a.tokens);
  }, [models, total]);

  const firstDay = stats?.first_day || "—";

  return (
    <div>
      <PageHead
        title={t("usage.title")}
        sub={t("usage.sub")}
        actions={
          <>
            <div className="segmented sm">
              {SCOPES.map((s) => (
                <button key={s} className={scope === s ? "active" : ""} onClick={() => setScope(s)}
                  title={t("usage.scopeLabel")}>{t(`usage.scope.${s}`)}</button>
              ))}
            </div>
            <IconAction
              icon={<IconRefresh />}
              label={t("usage.refresh")}
              onClick={() => load(period, scope)}
              disabled={!inTauri || refreshing}
              spinning={refreshing}
            />
          </>
        }
      />

      {err && <Err>{err}</Err>}

      <div className="usage-grid">
        {/* ── Side column ── */}
        <div className="usage-side">
          {/* Rolling stat strip */}
          <Card>
            <div className="ustat-strip">
              <StatCell loading={loading} val={fmtTokens(stats?.d7 ?? 0)} lab={t("usage.stat7d")} />
              <StatCell loading={loading} val={fmtTokens(stats?.d30 ?? 0)} lab={t("usage.stat30d")} />
              <StatCell loading={loading} val={fmtTokens(stats?.avg ?? 0)} lab={t("usage.statAvg")} />
              <StatCell loading={loading} val={String(data?.conversations ?? 0)} lab={t("usage.statConvs")} />
            </div>
            <div className="mrank">
              {loading ? (
                <div style={{ padding: "8px 0" }}><Skeleton h={14} style={{ marginBottom: "var(--s8)" }} /><Skeleton h={14} w="70%" /></div>
              ) : models.length === 0 ? (
                <div className="mrank-empty">{t("usage.noModels")}</div>
              ) : (
                // The swatch, not the number, carries the colour — it is what
                // ties the row to its slice of the distribution bar, and a
                // coloured percentage reads as a warning it is not.
                models.slice(0, 3).map((m, i) => (
                  <div key={m.model} className="mrank-row">
                    <span className="mr-idx">{i + 1}</span>
                    <span className="mr-swatch" style={{ background: rampColor(i) }} />
                    <span className="mr-name mono">{m.model}</span>
                    <span className="mr-pct">{m.share.toFixed(1)}%</span>
                  </div>
                ))
              )}
            </div>
            <div className="usage-foot">
              <span>{t("usage.firstUsed")} <b>{firstDay}</b></span>
              <span>{t("usage.activeDays")} <b>{stats?.active_days ?? 0}</b></span>
            </div>
          </Card>

          {/* Activity heatmap */}
          <Card icon={<IconUsage />} title={t("usage.heatmapTitle")}>
            {loading ? <Skeleton h={100} /> : <Heatmap cells={data?.heatmap ?? []} lang={i18n.language} weekdays={t("usage.weekdays")} less={t("usage.less")} more={t("usage.more")} />}
          </Card>

          {/* Usage trend */}
          <Card title={t("usage.trendTitle")}>
            {loading ? <Skeleton h={120} /> : <Trend daily={daily} lang={i18n.language} />}
          </Card>
        </div>

        {/* ── Main column ── */}
        <div className="usage-main">
          <Card>
            {/* Period tabs (refresh lives next to the page title) */}
            <div className="tabstrip usage-periods">
              {PERIODS.map((p) => (
                <button key={p} className={period === p ? "active" : ""} onClick={() => setPeriod(p)}>{t(`usage.period.${p}`)}</button>
              ))}
            </div>

            {/* Headline */}
            <div className="usage-headline">
              <div className="uh-label">{t("usage.totalLabel")}</div>
              {loading ? (
                <div className="uh-skel"><Skeleton w={260} h={60} r={14} /></div>
              ) : (
                <div className="uh-value" onClick={() => setFullFmt((v) => !v)} title={total.toLocaleString()}>
                  {fullFmt ? Math.round(animated).toLocaleString() : fmtTokens(animated)}
                </div>
              )}
              {!loading && (data?.total_amount ?? 0) > 0 && (
                <div className="uh-cost">{fmtUsdt(data!.total_amount)} USDT <IconInfo /></div>
              )}
            </div>

            {/* Distribution + cards */}
            {loading ? (
              <><Skeleton h={6} style={{ margin: "24px 0" }} /><Skeleton h={90} /></>
            ) : models.length === 0 ? (
              <div className="empty"><div className="empty-desc">{t("usage.noData")}</div></div>
            ) : (
              <div className="usage-dist">
                <div className="dist-bar" role="img" aria-label={t("usage.totalLabel")}>
                  {models.map((m, i) => (
                    <span key={m.model} style={{ width: `${m.share}%`, background: rampColor(i) }} title={`${m.model}: ${m.share.toFixed(1)}%`} />
                  ))}
                </div>
                <div className="uprov-grid">
                  <button
                    className={`uprov-card ${expandProv === "__all__" ? "active" : ""}`}
                    onClick={() => setExpandProv((p) => (p === "__all__" ? null : "__all__"))}
                  >
                    <div className="up-head"><IconUsage />{t("usage.allTools")}</div>
                    <div className="up-pct">100.00%</div>
                    <div className="up-sub">{t("usage.modelCount", { n: models.length })}</div>
                  </button>
                  {families.map((f, i) => (
                    <button
                      key={f.id}
                      className={`uprov-card ${expandProv === f.id ? "active" : ""}`}
                      onClick={() => setExpandProv((p) => (p === f.id ? null : f.id))}
                    >
                      <div className="up-head">
                        <span className="up-swatch" style={{ background: rampColor(i) }} />
                        {t(`usage.family.${f.id}`, { defaultValue: f.id.toUpperCase() })}
                      </div>
                      <div className="up-pct">{f.share.toFixed(2)}%</div>
                      <div className="up-sub">{t("usage.modelCount", { n: f.count })}</div>
                    </button>
                  ))}
                </div>

                {/* Expanded per-model rows */}
                {expandProv && (
                  <div className="fade-in umodel-list">
                    {models
                      .filter((m) => expandProv === "__all__" || FAMILY(m.model) === expandProv)
                      .map((m, i) => <ModelRow key={m.model} m={m} idx={i} t={t} />)}
                  </div>
                )}
              </div>
            )}
          </Card>

          {/* Daily breakdown table */}
          <Card title={t("usage.dailyTitle")}>
            {loading ? (
              <Skeleton h={140} />
            ) : daily.length === 0 ? (
              <div className="empty"><div className="empty-desc">{t("usage.noData")}</div></div>
            ) : (
              <div className="table-wrap tall">
                <table className="tbl">
                  <thead>
                    <tr>
                      <th>{t("usage.colDate")}</th>
                      <th className="num">{t("usage.colTotal")}</th>
                      <th className="num">{t("usage.colInput")}</th>
                      <th className="num">{t("usage.colOutput")}</th>
                      <th className="num">{t("usage.colCache")}</th>
                      <th className="num">{t("usage.colConvs")}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {[...daily].reverse().map((d) => (
                      <tr key={d.date}>
                        <td className="tabular">{d.date}</td>
                        <td className="mono tabular num">{d.total > 0 ? fmtTokens(d.total) : <span className="faint">—</span>}</td>
                        <td className="mono tabular num">{d.input > 0 ? fmtTokens(d.input) : <span className="faint">—</span>}</td>
                        <td className="mono tabular num">{d.output > 0 ? fmtTokens(d.output) : <span className="faint">—</span>}</td>
                        <td className="mono tabular num">{d.cache > 0 ? fmtTokens(d.cache) : <span className="faint">—</span>}</td>
                        <td className="mono tabular num">{d.count > 0 ? d.count : <span className="faint">—</span>}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </Card>
        </div>
      </div>
    </div>
  );
}

function StatCell({ loading, val, lab }: { loading: boolean; val: string; lab: string }) {
  return (
    <div className="ustat">
      <div className="us-val">{loading ? <Skeleton w={50} h={18} /> : val}</div>
      <div className="us-lab">{lab}</div>
    </div>
  );
}

function ModelRow({ m, idx, t }: { m: UsageModelRow; idx: number; t: (k: string, o?: Record<string, unknown>) => string }) {
  return (
    <div>
      <div className="umodel-head">
        <span className="mono umodel-name">{m.model}</span>
        <span className="muted">
          <span className="mono">{fmtTokens(m.tokens)}</span> · {m.share.toFixed(1)}% · {t("usage.convsN", { n: m.count })}
        </span>
      </div>
      <div className="bar">
        <span style={{ width: `${Math.min(100, m.share)}%`, background: rampColor(idx) }} />
      </div>
    </div>
  );
}

// ── Heatmap: GitHub-style 7×N grid over the last ~22 weeks ──
function Heatmap({ cells, lang, weekdays, less, more }: { cells: UsageHeatCell[]; lang: string; weekdays: string; less: string; more: string }) {
  const { weeks, months, max } = useMemo(() => buildHeatmap(cells, lang), [cells, lang]);
  const wd = weekdays.split(",");
  // The 22-week grid is wider than the narrow side column, so it scrolls.
  // Start scrolled to the far right so the most recent weeks (where activity
  // actually is) are visible by default — GitHub-style.
  const wrapRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = wrapRef.current;
    if (el) el.scrollLeft = el.scrollWidth;
  }, [weeks]);
  const level = (tokens: number) => {
    if (tokens <= 0 || max <= 0) return "";
    const r = tokens / max;
    return r > 0.66 ? "l4" : r > 0.4 ? "l3" : r > 0.15 ? "l2" : "l1";
  };
  return (
    <div>
      {/* Only the grid scrolls — the legend stays put, otherwise "less" scrolls
          out of sight along with the oldest weeks. */}
      <div className="heatmap-wrap" ref={wrapRef}>
        {/* Month labels */}
        <div className="hm-months">
          {weeks.map((_, ci) => {
            const label = months.find((m) => m.col === ci);
            return <div key={ci}>{label ? label.text : ""}</div>;
          })}
        </div>
        <div className="hm-body">
          {/* Weekday labels */}
          <div className="hm-days">
            {wd.map((d, i) => <div key={i}>{d}</div>)}
          </div>
          <div className="heatmap">
            {weeks.flatMap((week, ci) =>
              week.map((day, ri) => (
                <div
                  key={`${ci}-${ri}`}
                  className={`hm-cell ${day ? level(day.tokens) : ""}`}
                  style={day ? undefined : { visibility: "hidden" }}
                  title={day ? `${day.date} · ${fmtTokens(day.tokens)}` : undefined}
                />
              )),
            )}
          </div>
        </div>
      </div>
      <div className="hm-legend">
        <span>{less}</span>
        <span className="hm-cell" /><span className="hm-cell l1" /><span className="hm-cell l2" /><span className="hm-cell l3" /><span className="hm-cell l4" />
        <span>{more}</span>
      </div>
    </div>
  );
}

interface HeatDay { date: string; tokens: number }
function buildHeatmap(cells: UsageHeatCell[], lang: string) {
  const byDate = new Map(cells.map((c) => [c.date, c.tokens]));
  const WEEKS = 22;
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  // End on the most recent Saturday-complete week; start WEEKS*7 days back on a Sunday.
  const start = new Date(today);
  start.setDate(start.getDate() - (WEEKS * 7 - 1));
  start.setDate(start.getDate() - start.getDay()); // back to Sunday
  const weeks: (HeatDay | null)[][] = [];
  const months: { col: number; text: string }[] = [];
  let lastMonth = -1;
  const fmtMonth = new Intl.DateTimeFormat(lang, { month: "short" });
  let max = 0;
  const cursor = new Date(start);
  for (let c = 0; ; c++) {
    if (cursor > today && cursor.getDay() === 0) break;
    const col: (HeatDay | null)[] = [];
    for (let r = 0; r < 7; r++) {
      if (cursor > today) { col.push(null); }
      else {
        const y = cursor.getFullYear();
        const m = String(cursor.getMonth() + 1).padStart(2, "0");
        const d = String(cursor.getDate()).padStart(2, "0");
        const key = `${y}-${m}-${d}`;
        const tokens = byDate.get(key) ?? 0;
        if (tokens > max) max = tokens;
        col.push({ date: key, tokens });
        if (r === 0 && cursor.getMonth() !== lastMonth) {
          lastMonth = cursor.getMonth();
          months.push({ col: c, text: fmtMonth.format(cursor) });
        }
      }
      cursor.setDate(cursor.getDate() + 1);
    }
    weeks.push(col);
    if (c > 60) break; // safety
  }
  return { weeks, months, max };
}

// ── Trend: daily token bar chart over the selected period ──
function Trend({ daily, lang }: { daily: UsageDailyRow[]; lang: string }) {
  if (daily.length === 0) return <div className="trend-empty">—</div>;
  const max = Math.max(1, ...daily.map((d) => d.total));
  const fmt = (s: string) => {
    const dt = new Date(s + "T00:00:00");
    return new Intl.DateTimeFormat(lang, { month: "numeric", day: "numeric" }).format(dt);
  };
  return (
    <div>
      <div className="trend">
        {daily.map((d) => (
          <div key={d.date} className="tr-bar" style={{ height: `${Math.max(2, (d.total / max) * 100)}%` }} title={`${d.date} · ${fmtTokens(d.total)}`} />
        ))}
      </div>
      <div className="trend-axis">
        <span>{fmt(daily[0].date)}</span>
        <span>{fmt(daily[daily.length - 1].date)}</span>
      </div>
    </div>
  );
}
