// Time-aware pace + current-rate projection for usage-limit windows.
// Ported from TokenTracker's limit-pace.js — pure functions, no UI.

export type DisplayMode = "used" | "remaining";

/** Fraction (0..1) of the window elapsed by now, or null if inputs are unusable. */
export function expectedUsedFraction(windowSeconds: number, secondsUntilReset: number): number | null {
  if (!(windowSeconds > 0)) return null;
  const fraction = (windowSeconds - secondsUntilReset) / windowSeconds;
  if (!Number.isFinite(fraction)) return null;
  return Math.min(Math.max(fraction, 0), 1);
}

/** True when actual usage is meaningfully ahead of an even burn. */
export function isOverPace(usedFraction: number, expectedFraction: number, tolerance = 0.03): boolean {
  return usedFraction > expectedFraction + tolerance;
}

function durationString(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  const h = Math.floor(s / 3600);
  if (h > 24) return `${Math.floor(h / 24)}d`;
  if (h > 0) return `${h}h`;
  return `${Math.floor(s / 60)}m`;
}

/** Parse a window reset (ISO string or unix seconds) into epoch milliseconds. */
export function resetToMs(isoOrUnix: string | number | null | undefined): number {
  if (isoOrUnix == null) return NaN;
  return typeof isoOrUnix === "number" ? isoOrUnix * 1000 : Date.parse(isoOrUnix);
}

export interface Pace {
  pacePercent: number | null;
  paceOver: boolean;
  expectedPercent: number | null;
  runsOutEta: string | null;
  projectedEnd: number | null;
}

/**
 * Compute pace marker position + projection for one window.
 *   pacePercent   display-space marker position (0..100), or null to hide the mark
 *   paceOver      true = ahead of pace (deficit, red), false = on/under (green)
 *   expectedPercent  even-burn % by now (0..100), or null
 *   runsOutEta    "~3h" if projected to exhaust before reset, else null
 *   projectedEnd  projected % by reset (0..100) when it won't run out, else null
 */
export function computePace({
  usedPercent,
  windowSeconds,
  resetMs,
  mode,
  now = Date.now(),
}: {
  usedPercent: number | null | undefined;
  windowSeconds: number | null;
  resetMs: number;
  mode: DisplayMode;
  now?: number;
}): Pace {
  const usedFraction = Math.min(Math.max(Number(usedPercent) || 0, 0), 100) / 100;
  const out: Pace = { pacePercent: null, paceOver: false, expectedPercent: null, runsOutEta: null, projectedEnd: null };
  if (!(windowSeconds && windowSeconds > 0) || !Number.isFinite(resetMs)) return out;

  const secondsUntilReset = Math.max(0, (resetMs - now) / 1000);
  const expected = expectedUsedFraction(windowSeconds, secondsUntilReset);
  if (expected == null) return out;

  out.expectedPercent = Math.round(expected * 100);
  out.paceOver = isOverPace(usedFraction, expected);

  // Show the mark only once the window has meaningful usage (≥5%), so a fresh
  // window doesn't float a mark in the empty track.
  if (usedFraction >= 0.05) {
    const display = mode === "remaining" ? 1 - expected : expected;
    out.pacePercent = display * 100;
  }

  // Project at the current burn rate (rate = used / elapsed).
  if (expected > 0.02 && usedFraction > 0) {
    const elapsedSeconds = windowSeconds * expected;
    const ratePerSecond = usedFraction / elapsedSeconds;
    const projectedAtReset = usedFraction / expected;
    if (projectedAtReset >= 1 && ratePerSecond > 0) {
      out.runsOutEta = durationString((1 - usedFraction) / ratePerSecond);
    } else {
      out.projectedEnd = Math.round(Math.min(projectedAtReset, 1) * 100);
    }
  }

  return out;
}

/** Compact relative reset countdown: "5m" / "3h" / "2d", or the "now" word. */
export function formatReset(isoOrUnix: string | number | null | undefined, nowWord: string): string | null {
  const ts = resetToMs(isoOrUnix);
  if (!Number.isFinite(ts)) return null;
  const diff = ts - Date.now();
  if (diff <= 0) return nowWord;
  const m = Math.floor(diff / 60000);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.floor(h / 24)}d`;
}

/** Exact local reset time "M/D HH:mm" for hover/detail lines. */
export function formatExactReset(isoOrUnix: string | number | null | undefined, locale?: string): string | null {
  const ms = resetToMs(isoOrUnix);
  if (!Number.isFinite(ms)) return null;
  return new Intl.DateTimeFormat(locale, {
    month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit", hour12: false,
  }).format(new Date(ms));
}
