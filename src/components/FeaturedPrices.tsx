// What the market pays right now for the models the platform features — the
// overview's price ticker.
//
// The same panel the landing page draws (asale-web/src/components/home/
// FeaturedPrices.tsx), from the same endpoint and the same admin-chosen list.
// Two components rather than one for the reason the two world maps are two:
// different React majors, i18n libraries and design systems. The *data* is one
// request, batched server-side, so neither app maintains its own idea of which
// models matter.
//
// Everything expensive about this is already paid for elsewhere: the server
// batches the catalog lookup and 24h of history into one response and caches it
// for 10 seconds, and the daemon just forwards it. So the only cost decision
// left here is the poll interval — see POLL_MS.

import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, inTauri, type FeaturedModel, type FeaturedResp, type SparkPoint } from "../lib";
import { Card, Mark, Skeleton } from "../ui";
import { IconStore } from "../icons";

/**
 * How often the ticker refreshes.
 *
 * A minute, because the server's pricing loop reprices once a minute and
 * nothing this panel shows can move faster than that. The dashboard's other
 * pollers run at 4s because they watch local link state, which genuinely does
 * change that fast; copying that interval here would be fifteen HTTP round
 * trips a minute to redraw identical numbers.
 */
const POLL_MS = 60_000;

/** Below this a move is the pricing EMA's own noise, not a trend: shown flat,
 *  unsigned, in grey. Matches the web ticker so the two never disagree. */
const FLAT_BAND = 0.001;

/** Sparkline viewbox. The x axis is stretched to the card's width; only the
 *  aspect ratio and stroke weight come from these. */
const W = 240;
const H = 44;
const PAD = 3;
/** Narrowest y range a curve is drawn against, in ratio units — what stops a
 *  0.05% wobble from being rendered as a mountain range. */
const MIN_BAND = 0.04;

type Trend = "up" | "down" | "flat";

function trendOf(change: number | null): Trend {
  if (change === null || Math.abs(change) < FLAT_BAND) return "flat";
  return change > 0 ? "up" : "down";
}

/** A signed percentage, always signed. The dead band above greys the number;
 *  it does not strip the sign, because an unsigned "0.02%" for a −0.015% move
 *  leaves the reader unable to tell which way it went. Sign is read off the
 *  *rounded* value so a −0.001% move does not render as "−0.00%". */
function fmtChange(pct: number): string {
  const n = Number(pct.toFixed(2));
  return `${n > 0 ? "+" : n < 0 ? "−" : ""}${Math.abs(n).toFixed(2)}%`;
}

const COLOR: Record<Trend, string> = {
  up: "var(--success)",
  down: "var(--danger)",
  flat: "var(--fg-3)",
};

export function FeaturedPrices() {
  const { t } = useTranslation();
  const [models, setModels] = useState<FeaturedModel[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    if (!inTauri) { setLoading(false); return; }
    let alive = true;
    const load = () =>
      invoke<FeaturedResp>("market_featured")
        .then((r) => { if (alive) setModels(r.models || []); })
        // Silent: this panel is context, not state the reader is acting on, and
        // a red banner over a price ticker on the overview page would be a
        // louder failure than the missing prices warrant.
        .catch(() => {})
        .finally(() => { if (alive) setLoading(false); });
    void load();
    const id = setInterval(load, POLL_MS);
    return () => { alive = false; clearInterval(id); };
  }, []);

  if (loading) {
    return (
      <Card icon={<IconStore />} title={t("dashboard.prices.title")}>
        <div className="fp-grid">
          {Array.from({ length: 4 }, (_, i) => (
            <div key={i} className="fp-card"><Skeleton h={92} r={10} /></div>
          ))}
        </div>
      </Card>
    );
  }

  // Nothing configured, or the server is unreachable: the panel's whole content
  // is live numbers, so it removes itself rather than sitting there as a row of
  // dashes.
  if (models.length === 0) return null;

  return (
    <Card
      icon={<IconStore />}
      title={t("dashboard.prices.title")}
      desc={t("dashboard.prices.sub")}
    >
      <div className="fp-grid">
        {models.map((m) => <PriceCard key={m.model} m={m} />)}
      </div>
    </Card>
  );
}

function PriceCard({ m }: { m: FeaturedModel }) {
  const { t } = useTranslation();
  const trend = trendOf(m.change_24h);
  const pct = m.change_24h === null ? null : m.change_24h * 100;
  // Output tokens: the price that dominates a real bill, and the one the market
  // page leads with — so both surfaces mean the same thing by "the price".
  const price = (m.market_prices.output / 1000).toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 4,
  });

  return (
    <div className="fp-card">
      <div className="fp-top">
        <Mark id={m.provider} size="sm" />
        <span className="fp-name mono" title={m.display_name}>{m.model}</span>
        <span className="fp-off">{Math.round(m.discount * 100)}%</span>
      </div>
      <div className="fp-price">
        <span className="mono tabular">{price}</span>
        <span className="fp-unit">{t("dashboard.prices.perM")}</span>
        <span className="fp-chg mono tabular" style={{ color: COLOR[trend] }}>
          {pct === null ? t("dashboard.prices.new") : fmtChange(pct)}
        </span>
      </div>
      <Spark points={m.points} trend={trend} />
    </div>
  );
}

function Spark({ points, trend }: { points: SparkPoint[]; trend: Trend }) {
  const geom = useMemo(() => {
    if (points.length < 2) return null;
    const ratios = points.map((p) => p.ratio);
    const lo = Math.min(...ratios);
    const hi = Math.max(...ratios);
    // The band is centred on the data so a quiet series sits mid-box rather
    // than pinned to the bottom of an artificially widened range.
    const mid = (lo + hi) / 2;
    const half = Math.max((hi - lo) / 2, MIN_BAND / 2);
    const top = mid + half;
    const span = half * 2;
    const plotW = W - PAD * 2;
    const plotH = H - PAD * 2;
    const x = (i: number) => PAD + (i / (points.length - 1)) * plotW;
    const y = (r: number) => PAD + ((top - r) / span) * plotH;
    const line = points
      .map((p, i) => `${i === 0 ? "M" : "L"}${x(i).toFixed(1)},${y(p.ratio).toFixed(1)}`)
      .join(" ");
    return {
      line,
      area: `${line} L${x(points.length - 1).toFixed(1)},${H} L${PAD},${H} Z`,
      endX: x(points.length - 1).toFixed(1),
      endY: y(ratios[ratios.length - 1]).toFixed(1),
    };
  }, [points]);

  // One point is a dot, not a line. The box is still reserved so a model with
  // no history yet does not make its card shorter than the others.
  if (!geom) return <div className="fp-spark" />;
  const color = COLOR[trend];

  return (
    <svg className="fp-spark" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" aria-hidden>
      <path d={geom.area} fill={color} opacity={0.09} />
      {/* vectorEffect keeps the stroke out of the x-axis stretch, so the line is
          the same weight on a wide card as on a narrow one. */}
      <path
        d={geom.line}
        fill="none"
        stroke={color}
        strokeWidth={1.5}
        strokeLinecap="round"
        strokeLinejoin="round"
        vectorEffect="non-scaling-stroke"
      />
      {/* "Now", as a zero-length path with a round cap rather than a <circle>:
          a circle would be stretched into an ellipse by the same x-axis scale
          the stroke is exempt from. */}
      <path
        d={`M${geom.endX},${geom.endY} l0,0`}
        stroke={color}
        strokeWidth={4}
        strokeLinecap="round"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}
