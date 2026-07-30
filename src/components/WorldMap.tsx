// Country-level choropleth of where the network's users are, with one arc per
// seller-country → buyer-country token lane — and the signed-in user's own
// country called out on top of it.
//
// The same picture the landing page draws (asale-web/src/components/home/
// WorldMap.tsx). The two are separate components on purpose — different React
// majors, i18n libraries and design systems — but the *data* they draw is one
// endpoint and the outlines are one generator: `asale-web/scripts/gen-world.mjs`
// writes ../lib/world.ts here and its twin there, so neither app needs a
// mapping library at runtime.

import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, inTauri, isDaemonDown, fmtTokens, type Globe, type GlobeFlow, type GlobeRegion } from "../lib";
import { COUNTRY_CENTROIDS, COUNTRY_SHAPES, WORLD_H, WORLD_W } from "../lib/world";
import { errText } from "../errors";
import { Card, Skeleton } from "../ui";
import { IconAccount, IconAlert, IconGlobe, IconUsage, IconZap } from "../icons";

/** More arcs than this and the map reads as a hairball rather than a network. */
const MAX_ARCS = 24;
/** Fixed so the hover box can be clamped without measuring it. */
const TIP_W = 172;

interface Hover {
  code: string;
  x: number;
  y: number;
}

/** ISO-3166 alpha-2 → regional-indicator flag emoji. "ZZ"/invalid → globe. */
function regionFlag(code: string): string {
  const c = code.trim().toUpperCase();
  if (!/^[A-Z]{2}$/.test(c) || c === "ZZ") return "🌐";
  return String.fromCodePoint(...[...c].map((ch) => 0x1f1e6 + ch.charCodeAt(0) - 65));
}

/**
 * `region` is the country the signed-in user declared (`users.region`) — the
 * only location the platform has, since nothing here reads an IP. Empty when
 * signed out or never set, and then nothing is highlighted.
 */
export function WorldMap({ region = "" }: { region?: string }) {
  const { t, i18n } = useTranslation();
  const [regions, setRegions] = useState<GlobeRegion[]>([]);
  const [flows, setFlows] = useState<GlobeFlow[]>([]);
  const [err, setErr] = useState("");
  const [loading, setLoading] = useState(true);
  const [hover, setHover] = useState<Hover | null>(null);
  const wrap = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!inTauri) { setLoading(false); return; }
    let alive = true;
    const load = () =>
      invoke<Globe>("market_globe")
        .then((r) => {
          if (!alive) return;
          setRegions(r.regions ?? []);
          setFlows(r.flows ?? []);
          setErr("");
        })
        // A dead daemon is the page's own banner to raise, not this card's.
        .catch((e) => { if (alive && !isDaemonDown(e)) setErr(errText(e)); })
        .finally(() => { if (alive) setLoading(false); });
    load();
    // Country membership moves in hours, not seconds: polling this at the
    // dashboard's 4s cadence would be fifteen pointless round trips a minute.
    const id = setInterval(load, 60_000);
    return () => { alive = false; clearInterval(id); };
  }, []);

  const regionName = useMemo(() => {
    let dn: Intl.DisplayNames | null = null;
    try {
      dn = new Intl.DisplayNames([i18n.language], { type: "region" });
    } catch {
      dn = null;
    }
    return (code: string) => {
      if (code === "ZZ") return t("dashboard.map.unknownRegion");
      try {
        return dn?.of(code) || code;
      } catch {
        return code;
      }
    };
  }, [i18n.language, t]);

  const byCode = useMemo(() => {
    const m = new Map<string, GlobeRegion>();
    for (const r of regions) m.set(r.region.toUpperCase(), r);
    return m;
  }, [regions]);

  const shapeOf = useMemo(() => new Map(COUNTRY_SHAPES.map(([code, d]) => [code, d])), []);

  /** The user's country, once the map has somewhere to put it. A centroid is
   *  enough: countries too small to outline at this scale (Singapore, Hong
   *  Kong, …) still get the pin, which is the part that says "you". */
  const mine = useMemo(() => {
    const c = region.trim().toUpperCase();
    return COUNTRY_CENTROIDS[c] ? c : "";
  }, [region]);

  // Users per country is heavily skewed — one hub country would flatten every
  // other shade to the palest step on a linear ramp, so the ramp is log-scaled.
  // The exponent then pulls the long tail back down: without it a country with
  // a dozen users already lands halfway up the ramp and the map reads as flat.
  const shade = useMemo(() => {
    // ZZ never gets a shape, so letting it into the max only darkens nothing
    // while washing out every country that is actually drawn.
    const max = Math.max(1, ...regions.filter((r) => r.region !== "ZZ").map((r) => r.users));
    const denom = Math.log1p(max);
    return (users: number) => (users > 0 ? (Math.log1p(users) / denom) ** 2.2 : 0);
  }, [regions]);

  // Cross-border lanes only: an arc that starts and ends on the same centroid
  // renders as a dot, and self-serve traffic is not what the map is about.
  const arcs = useMemo(() => {
    const cross = flows.filter(
      (f) => f.from !== f.to && COUNTRY_CENTROIDS[f.from] && COUNTRY_CENTROIDS[f.to]
    );
    const max = Math.max(1, ...cross.map((f) => f.tokens));
    return cross.slice(0, MAX_ARCS).map((f) => ({
      ...f,
      d: arcPath(COUNTRY_CENTROIDS[f.from], COUNTRY_CENTROIDS[f.to]),
      weight: f.tokens / max,
    }));
  }, [flows]);

  const totals = useMemo(() => {
    // Countries and users share one denominator — the regions actually on the
    // map — so the pair never reads "1 country / 3 users" with no shape to
    // account for the difference. Throughput keeps every region: a lane is real
    // traffic whether or not both ends declared a country.
    const named = regions.filter((r) => r.region !== "ZZ");
    return {
      countries: named.length,
      users: named.reduce((a, r) => a + r.users, 0),
      tokens: regions.reduce((a, r) => a + r.tokens, 0),
      lanes: flows.length,
    };
  }, [regions, flows]);

  const hovered = hover ? byCode.get(hover.code) : null;

  /** Pointer → tooltip position, clamped inside the frame while we hold the rect. */
  function place(e: React.MouseEvent, code: string, known: boolean) {
    if (!known || !wrap.current) return;
    const box = wrap.current.getBoundingClientRect();
    setHover({
      code,
      x: Math.max(6, Math.min(e.clientX - box.left + 14, box.width - TIP_W)),
      y: Math.max(6, e.clientY - box.top - 60),
    });
  }

  return (
    <Card
      icon={<IconGlobe />}
      title={t("dashboard.map.title")}
      desc={t("dashboard.map.desc")}
      className="map-card"
    >
      {err && (
        <div className="callout danger card-lead"><IconAlert /><span>{err}</span></div>
      )}

      <div ref={wrap} className="map-frame" onMouseLeave={() => setHover(null)}>
        <svg
          viewBox={`0 0 ${WORLD_W} ${WORLD_H}`}
          className={`map-svg${loading ? " is-loading" : ""}`}
          role="img"
          aria-label={t("dashboard.map.title")}
        >
          <defs>
            {/* Arcs fade in from the seller and land bright on the buyer, so
                direction reads without arrowheads cluttering the map. */}
            <linearGradient id="asale-lane" x1="0" y1="0" x2="1" y2="0">
              <stop offset="0%" stopColor="var(--accent)" stopOpacity="0.05" />
              <stop offset="45%" stopColor="var(--accent-hover)" stopOpacity="0.75" />
              <stop offset="100%" stopColor="var(--accent-hover)" stopOpacity="0.95" />
            </linearGradient>
          </defs>

          <g strokeWidth={0.4} stroke="var(--map-edge)">
            {COUNTRY_SHAPES.map(([code, d]) => {
              const r = byCode.get(code);
              const s = r ? shade(r.users) : 0;
              return (
                <path
                  key={code}
                  d={d}
                  fill={
                    s > 0
                      ? `color-mix(in srgb, var(--accent) ${Math.round(14 + s * 70)}%, var(--map-land))`
                      : "var(--map-land)"
                  }
                  className={r ? "is-live" : undefined}
                  onMouseEnter={(e) => place(e, code, !!r)}
                  onMouseMove={(e) => place(e, code, !!r)}
                />
              );
            })}
          </g>

          <g fill="none" stroke="url(#asale-lane)" strokeLinecap="round">
            {arcs.map((f, i) => (
              <g key={`${f.from}-${f.to}`}>
                <path d={f.d} strokeWidth={0.5 + f.weight * 1.7} opacity={0.28 + f.weight * 0.6} />
                {/* One packet per lane, offset so they never march in lockstep. */}
                <circle r={1.1 + f.weight * 1.4} fill="var(--accent-hover)" opacity={0.9}>
                  <animateMotion
                    dur={`${4.5 + (i % 5) * 0.9}s`}
                    begin={`${(i % 7) * 0.55}s`}
                    repeatCount="indefinite"
                    path={f.d}
                  />
                </circle>
              </g>
            ))}
          </g>

          <g pointerEvents="none">
            {regions
              .filter((r) => r.users > 0 && COUNTRY_CENTROIDS[r.region.toUpperCase()])
              .slice(0, 40)
              .map((r) => {
                const [cx, cy] = COUNTRY_CENTROIDS[r.region.toUpperCase()];
                return (
                  <circle
                    key={r.region}
                    cx={cx}
                    cy={cy}
                    r={1.4 + shade(r.users) * 2.6}
                    fill="var(--accent-hover)"
                    opacity={0.55}
                  />
                );
              })}
          </g>

          {/* The user's own country, drawn last so its outline is never
              overpainted by a neighbour's edge — and as an overlay rather than a
              special case in the choropleth pass, which keeps the shading of
              every other country exactly as it was. */}
          {mine && <YouAreHere code={mine} d={shapeOf.get(mine)} />}
        </svg>

        {hovered && hover && (
          <div className="map-tip" style={{ left: hover.x, top: hover.y, width: TIP_W }}>
            <div className="mt-name">
              <span aria-hidden="true">{regionFlag(hovered.region)}</span>
              {regionName(hovered.region)}
            </div>
            <div className="mt-line tabular">{t("dashboard.map.tipUsers", { count: hovered.users })}</div>
            {hovered.tokens > 0 && (
              <div className="mt-line faint tabular">
                {t("dashboard.map.tipTokens", { tokens: fmtTokens(hovered.tokens) })}
              </div>
            )}
            {hovered.region.toUpperCase() === mine && (
              <div className="mt-you">{t("dashboard.map.tipYou")}</div>
            )}
          </div>
        )}
      </div>

      {/* Legend + totals share the footer so the map keeps the full frame. */}
      <div className="map-foot">
        <div className="map-legend">
          <span className="mf-label">{t("dashboard.map.legendUsers")}</span>
          <span className="mf-hint">{t("dashboard.map.legendLess")}</span>
          <span className="map-ramp">
            {[0, 0.25, 0.5, 0.75, 1].map((s) => (
              <span
                key={s}
                style={{ background: `color-mix(in srgb, var(--accent) ${Math.round(14 + s * 70)}%, var(--map-land))` }}
              />
            ))}
          </span>
          <span className="mf-hint">{t("dashboard.map.legendMore")}</span>
        </div>

        <div className="map-legend">
          <span className="mf-label">{t("dashboard.map.legendFlow")}</span>
          <svg width="52" height="12" aria-hidden="true">
            <path d="M2 9 Q26 -3 50 6" fill="none" stroke="url(#asale-lane)" strokeWidth="2" strokeLinecap="round" />
          </svg>
        </div>

        {mine && (
          <div className="map-legend">
            <span className="mf-label">{t("dashboard.map.legendYou")}</span>
            {/* The same mark the map draws, so the legend explains it rather
                than restating the country name in a second colour. */}
            <svg width="14" height="14" aria-hidden="true">
              <circle cx="7" cy="7" r="2.1" fill="var(--fg)" />
              <circle cx="7" cy="7" r="5.6" fill="none" stroke="var(--fg)" strokeWidth="1" opacity="0.45" />
            </svg>
            <span className="map-you-chip">
              <span aria-hidden="true">{regionFlag(mine)}</span>
              {regionName(mine)}
            </span>
          </div>
        )}

        <dl className="map-stats">
          <Metric icon={<IconGlobe />} label={t("dashboard.map.statCountries")} value={String(totals.countries)} loading={loading} />
          <Metric icon={<IconAccount />} label={t("dashboard.map.statUsers")} value={totals.users.toLocaleString()} loading={loading} />
          <Metric icon={<IconUsage />} label={t("dashboard.map.statTokens")} value={fmtTokens(totals.tokens)} loading={loading} />
          <Metric icon={<IconZap />} label={t("dashboard.map.statLanes")} value={String(totals.lanes)} loading={loading} />
        </dl>
      </div>
    </Card>
  );
}

/** Outline + pulsing pin over the user's country. `d` is absent for a country
 *  too small to have an outline at this scale, and then the pin is the marker.
 *
 *  Drawn in the neutral extreme rather than in the accent, for two reasons: the
 *  accent is already the choropleth's own scale, so a brighter accent here would
 *  read as "more users" and be invisible next to a country that genuinely has
 *  them; and this is an annotation, not data. Against every shade of the ramp,
 *  in both themes, `--fg` is the one value that always contrasts. */
function YouAreHere({ code, d }: { code: string; d?: string }) {
  const [cx, cy] = COUNTRY_CENTROIDS[code];
  return (
    <g className="map-you" pointerEvents="none">
      {d && (
        <>
          {/* A wide soft stroke under a crisp one: the halo reads at country
              scale, the hairline keeps the border exact even for a small island. */}
          <path d={d} fill="none" stroke="var(--fg)" strokeWidth={3.6} opacity={0.16} />
          <path
            d={d}
            fill="color-mix(in srgb, var(--fg) 12%, transparent)"
            stroke="var(--fg)"
            strokeWidth={1.1}
          />
        </>
      )}
      {/* Without an outline to carry it, the pin gets a ring of its own so it
          still reads as a marker rather than as one of the density dots. */}
      {!d && <circle cx={cx} cy={cy} r={4.6} fill="none" stroke="var(--fg)" strokeWidth={1.1} opacity={0.6} />}
      <circle cx={cx} cy={cy} r={2.1} fill="var(--fg)" />
      <circle cx={cx} cy={cy} r={2.1} fill="none" stroke="var(--fg)" strokeWidth={1}>
        <animate attributeName="r" values="2.1;10" dur="2.6s" repeatCount="indefinite" />
        <animate attributeName="opacity" values="0.75;0" dur="2.6s" repeatCount="indefinite" />
      </circle>
    </g>
  );
}

function Metric({ icon, label, value, loading }: { icon: React.ReactNode; label: string; value: string; loading: boolean }) {
  return (
    <div className="map-stat">
      <dt>{icon}{label}</dt>
      <dd className="tabular">{loading ? <Skeleton w={40} h={17} r={6} /> : value}</dd>
    </div>
  );
}

/**
 * A quadratic arc bowed perpendicular to the lane. The bow scales with distance
 * so neighbouring countries get a gentle hop and transpacific lanes a tall one.
 */
function arcPath([x1, y1]: readonly [number, number], [x2, y2]: readonly [number, number]): string {
  const dx = x2 - x1;
  const dy = y2 - y1;
  const dist = Math.hypot(dx, dy) || 1;
  const lift = Math.min(dist * 0.28, 110);
  // Always bow "upwards" on screen: a consistent curvature keeps crossing lanes
  // readable, and the gradient still carries direction.
  const cx = (x1 + x2) / 2 + (dy / dist) * lift * (dx >= 0 ? 1 : -1);
  const cy = (y1 + y2) / 2 - (dx / dist) * lift * (dx >= 0 ? 1 : -1);
  return `M${x1} ${y1} Q${cx.toFixed(1)} ${cy.toFixed(1)} ${x2} ${y2}`;
}
