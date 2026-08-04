// What the market pays right now for the models the platform features — the
// bottom band of the overview's network panel.
//
// The same strip the landing page draws (asale-web/src/components/home/
// FeaturedPrices.tsx), from the same endpoint and the same admin-chosen list.
// Two components rather than one for the reason the two world maps are two:
// different React majors, i18n libraries and design systems. The *data* is one
// request, batched server-side, so neither app maintains its own idea of which
// models matter.
//
// It renders as a row inside the map's card, not a card of its own. A panel of
// four price tiles above the map made the overview two blocks that were saying
// one thing — here is the market, here is where it is — and the prices took
// three lines each to say what a name, a share and a shape say in one.
//
// Everything expensive about this is already paid for elsewhere: the server
// batches the catalog lookup and 24h of history into one response and caches it
// for 10 seconds, and the daemon just forwards it. So the only cost decision
// left here is the poll interval — see POLL_MS.

import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, inTauri, type FeaturedModel, type FeaturedResp, type SparkPoint } from "../lib";
import { Mark } from "../ui";

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
 *  in grey. Matches the web ticker so the two never disagree. */
const FLAT_BAND = 0.001;

/** Sparkline viewbox. The x axis is stretched to the tile's width; only the
 *  aspect ratio comes from these. */
const W = 240;
const H = 44;
/** Breathing room above the highest point and under the lowest, so a peak does
 *  not run flush into the label above and a trough still has visible fill. */
const PAD = 4;
const FLOOR = 6;
/** Narrowest y range a curve is drawn against, in ratio units — what stops a
 *  0.05% wobble from being rendered as a mountain range. */
const MIN_BAND = 0.04;

type Trend = "up" | "down" | "flat";

function trendOf(change: number | null): Trend {
  if (change === null || Math.abs(change) < FLAT_BAND) return "flat";
  return change > 0 ? "up" : "down";
}

const COLOR: Record<Trend, string> = {
  up: "var(--success)",
  down: "var(--danger)",
  flat: "var(--fg-3)",
};

export function FeaturedPrices() {
  const [models, setModels] = useState<FeaturedModel[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    if (!inTauri) { setLoading(false); return; }
    let alive = true;
    const load = () =>
      invoke<FeaturedResp>("market_featured")
        .then((r) => { if (alive) setModels(r.models || []); })
        // Silent: this row is context, not state the reader is acting on, and a
        // red banner under a world map would be a louder failure than the
        // missing prices warrant.
        .catch(() => {})
        .finally(() => { if (alive) setLoading(false); });
    void load();
    const id = setInterval(load, POLL_MS);
    return () => { alive = false; clearInterval(id); };
  }, []);

  // The band's own height while the first response is in flight, so the card
  // does not grow under the reader when the prices land. Empty rather than
  // shimmering: it is one row at the foot of a panel whose main content — the
  // map — has already arrived, and a second animation there would be noise.
  if (loading) {
    return (
      <div className="map-ticker" aria-hidden>
        {Array.from({ length: 4 }, (_, i) => <div key={i} className="tk-tile" />)}
      </div>
    );
  }

  // Nothing configured, or the server is unreachable: the row's whole content
  // is live numbers, so it removes itself rather than sitting under the map as
  // four empty tiles.
  if (models.length === 0) return null;

  return (
    <div className="map-ticker">
      {models.map((m) => <Tick key={m.model} m={m} />)}
    </div>
  );
}

function Tick({ m }: { m: FeaturedModel }) {
  const { t } = useTranslation();
  const trend = trendOf(m.change_24h);
  // What you pay as a share of the vendor's own rate — the inverse of the
  // discount, and the only figure on the tile. Measured on output tokens, the
  // price that dominates a real bill and the one the market board leads with,
  // so every surface agrees on what "the price" of a model means.
  const ofList = Math.max(0, Math.round((1 - m.discount) * 100));

  return (
    <div className="tk-tile">
      <Spark id={m.model} points={m.points} trend={trend} />
      <span className="tk-face">
        <Mark id={m.provider} size="sm" />
        <span className="tk-name mono" title={m.display_name}>{m.model}</span>
        {/* Unlabelled by design — the full sentence is the tile's title, and
            for a pointer-less reader every tile in the row is showing the same
            thing. */}
        <span className="tk-pct mono tabular" title={t("dashboard.prices.ofList")}>{ofList}%</span>
      </span>
    </div>
  );
}

/**
 * The day's curve as a filled area, stretched across the whole tile and dropped
 * behind the text.
 *
 * Area, not line. Drawn as the ground under a row of text, a 1.5px stroke was a
 * hairline running the width of the tile — on a quiet day a perfectly straight
 * one, indistinguishable from a rule someone had drawn there. The filled shape
 * says the same thing with no edge to mistake for a border.
 */
function Spark({ id, points, trend }: { id: string; points: SparkPoint[]; trend: Trend }) {
  const area = useMemo(() => {
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
    const plotH = H - PAD - FLOOR;
    const x = (i: number) => (i / (points.length - 1)) * W;
    const y = (r: number) => PAD + ((top - r) / span) * plotH;
    const ridge = points
      .map((p, i) => `${i === 0 ? "M" : "L"}${x(i).toFixed(1)},${y(p.ratio).toFixed(1)}`)
      .join(" ");
    // Closed down to the bottom of the box: with no stroke over it this shape
    // is the whole chart, so its top edge is the value and its depth is weight.
    return `${ridge} L${W},${H} L0,${H} Z`;
  }, [points]);

  // One point is a dot, not a shape. The box is still reserved so a model with
  // no history yet does not make its tile shorter than the others.
  if (!area) return <div className="tk-chart" />;

  const tint = COLOR[trend];
  // Ids may not start with a digit and may not contain whitespace; model names
  // do both. Everything outside the safe set becomes a dash. Two tiles sharing
  // an id would both take the first one's colours — the gradient is a
  // referenced paint server, not a local style.
  const fillId = `tk-fill-${id.replace(/[^a-zA-Z0-9_-]+/g, "-")}`;

  return (
    <svg className="tk-chart" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" aria-hidden>
      {/* The fill fades downwards instead of sitting at one flat opacity: a
          quiet day draws a nearly straight ridge, and under a flat wash that
          reads as a rectangle someone forgot to fill in. */}
      <defs>
        <linearGradient id={fillId} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor={tint} stopOpacity={0.42} />
          <stop offset="100%" stopColor={tint} stopOpacity={0.04} />
        </linearGradient>
      </defs>
      <path d={area} fill={`url(#${fillId})`} />
    </svg>
  );
}
