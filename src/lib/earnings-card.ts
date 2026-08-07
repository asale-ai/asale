// The earnings card: a screenshot-quality PNG of what an account has earned,
// drawn on a canvas in the browser.
//
// KEEP IN SYNC with asale-web/src/lib/earnings-card.ts.
//
// Why a canvas and not a server-rendered image. The numbers on this card are
// the account's own earnings, so a server route would need the session, and the
// desktop client would need the network — it draws this from a local SQLite
// store and is useful offline. Drawing in the page needs neither, and it hands
// back a real `File` that `navigator.share` can pass to the OS share sheet,
// which a remote URL cannot.
//
// The card is dark in both themes. It is not part of the page it was generated
// from — it lands in a feed, a group chat, a screenshot — so it has to be one
// fixed object with one look, rather than something that arrives light for one
// sharer and dark for the next.

import QRCode from "qrcode";

import { MARK_BOX, MARK_GROUP, MARK_PATHS } from "./mark";

export type CardShape = "wide" | "tall";

export interface CardStat {
  label: string;
  value: string;
}

export interface CardData {
  /** What the big number is: "共享闲置额度收益", "邀请返佣". */
  kicker: string;
  /** Formatted, without the unit: "12.34". */
  amount: string;
  unit: string;
  /** "本月", "累计" — sits in the pill beside the wordmark. */
  period: string;
  /** Up to three. Beyond that they stop being readable at feed size. */
  stats: CardStat[];
  tagline: string;
  /** Invite link, encoded into the QR. Empty draws the card without one. */
  link: string;
  code: string;
  codeHint: string;
  /** Wordmark text, e.g. "asale.ai". */
  site: string;
}

/** `wide` unfurls as a link preview (X, Telegram, Facebook, LinkedIn) at the
 *  1.91:1 every one of them crops to. `tall` is for the feeds that are shared
 *  into as an image — Xiaohongshu, Moments, Instagram — where a 1.91:1 card
 *  arrives as a letterboxed sliver. */
export const CARD_SIZE: Record<CardShape, { w: number; h: number }> = {
  wide: { w: 1200, h: 630 },
  tall: { w: 1080, h: 1350 },
};

// Fonts are named, never loaded: the card is drawn with whatever the OS has, so
// a Chinese caption renders in PingFang/Microsoft YaHei and a Japanese one in
// Hiragino, without this module shipping a 10 MB CJK webfont to every visitor.
const SANS =
  '"Inter", system-ui, -apple-system, "Segoe UI", "PingFang SC", "Hiragino Sans", "Microsoft YaHei", "Noto Sans CJK SC", sans-serif';
const MONO = 'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, monospace';

const INK = "#ffffff";
const INK_SOFT = "rgba(255,255,255,0.72)";
const INK_FAINT = "rgba(255,255,255,0.46)";
const ACCENT = "#8b89f0";

/**
 * Draw the card at full resolution.
 *
 * The canvas is resized to the shape's own pixel size rather than to the
 * element's CSS size × DPR: this output is an image file, and it must be the
 * same 1200×630 whether it was generated on a Retina laptop or a 1× monitor.
 * The preview scales the element down with CSS instead.
 */
export function drawEarningsCard(canvas: HTMLCanvasElement, d: CardData, shape: CardShape): void {
  const { w, h } = CARD_SIZE[shape];
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  const tall = shape === "tall";
  const pad = tall ? 84 : 64;

  background(ctx, w, h);

  // ── Header: mark, wordmark, period pill ────────────────────────────
  const markH = tall ? 52 : 42;
  const markW = (markH * MARK_BOX.w) / MARK_BOX.h;
  drawMark(ctx, pad, pad, markH, INK);

  const siteSize = tall ? 34 : 28;
  ctx.font = `600 ${siteSize}px ${SANS}`;
  ctx.fillStyle = INK;
  ctx.textBaseline = "middle";
  ctx.fillText(d.site, pad + markW + (tall ? 20 : 16), pad + markH / 2);

  if (d.period) pill(ctx, w - pad, pad + markH / 2, d.period, tall ? 26 : 22);

  // ── Hero: what the number is, then the number ──────────────────────
  ctx.textBaseline = "alphabetic";
  const heroTop = pad + markH + (tall ? 200 : 76);

  const kickerSize = tall ? 38 : 30;
  ctx.font = `500 ${kickerSize}px ${SANS}`;
  ctx.fillStyle = INK_SOFT;
  ctx.fillText(d.kicker, pad, heroTop + kickerSize);

  // The amount and its unit sit on one baseline, so a long figure pushes the
  // unit right instead of colliding with it. Column width excludes the QR on
  // the wide card, where the two share a row.
  const qrSide = tall ? 240 : 176;
  const heroWidth = tall ? w - pad * 2 : w - pad * 2 - qrSide - 56;
  const unitSize = tall ? 46 : 36;
  ctx.font = `600 ${unitSize}px ${SANS}`;
  const unitW = d.unit ? ctx.measureText(` ${d.unit}`).width : 0;

  const amountSize = fitText(ctx, d.amount, heroWidth - unitW, tall ? 168 : 124, "700");
  const amountBaseline = heroTop + kickerSize + (tall ? 44 : 30) + amountSize * 0.78;
  ctx.font = `700 ${amountSize}px ${SANS}`;
  ctx.fillStyle = INK;
  ctx.fillText(d.amount, pad, amountBaseline);
  const amountW = ctx.measureText(d.amount).width;
  if (d.unit) {
    ctx.font = `600 ${unitSize}px ${SANS}`;
    ctx.fillStyle = ACCENT;
    ctx.fillText(` ${d.unit}`, pad + amountW, amountBaseline);
  }

  // ── Stats ──────────────────────────────────────────────────────────
  const stats = d.stats.slice(0, 3);
  const statsTop = amountBaseline + (tall ? 140 : 74);
  if (stats.length) {
    const labelSize = tall ? 24 : 20;
    const valueSize = tall ? 44 : 34;
    // Split the hero column evenly. Three short stats read better in a row
    // than stacked, at both sizes.
    const colW = (tall ? w - pad * 2 : w - pad * 2 - qrSide - 56) / stats.length;
    ctx.strokeStyle = "rgba(255,255,255,0.10)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(pad, statsTop - (tall ? 52 : 38));
    ctx.lineTo(pad + colW * stats.length, statsTop - (tall ? 52 : 38));
    ctx.stroke();

    stats.forEach((s, i) => {
      const x = pad + colW * i;
      ctx.font = `500 ${labelSize}px ${SANS}`;
      ctx.fillStyle = INK_FAINT;
      ctx.fillText(s.label, x, statsTop);
      ctx.font = `600 ${valueSize}px ${SANS}`;
      ctx.fillStyle = INK;
      ctx.fillText(s.value, x, statsTop + valueSize + (tall ? 16 : 12));
    });
  }

  // ── QR and code ────────────────────────────────────────────────────
  // Wide: bottom-right, sharing the row with the tagline. Tall: centred under
  // everything, at the size a phone camera actually wants.
  const qrX = tall ? (w - qrSide) / 2 : w - pad - qrSide;
  const qrY = tall ? h - pad - qrSide - 96 : h - pad - qrSide - 44;
  if (d.link) {
    drawQr(ctx, d.link, qrX, qrY, qrSide);
    const codeSize = tall ? 32 : 26;
    ctx.textAlign = "center";
    ctx.font = `500 ${tall ? 24 : 20}px ${SANS}`;
    ctx.fillStyle = INK_FAINT;
    ctx.fillText(d.codeHint, qrX + qrSide / 2, qrY + qrSide + (tall ? 44 : 34));
    ctx.font = `700 ${codeSize}px ${MONO}`;
    ctx.fillStyle = INK;
    ctx.fillText(d.code, qrX + qrSide / 2, qrY + qrSide + (tall ? 84 : 66));
    ctx.textAlign = "left";
  }

  // ── Tagline ────────────────────────────────────────────────────────
  const taglineSize = tall ? 30 : 24;
  ctx.font = `500 ${taglineSize}px ${SANS}`;
  ctx.fillStyle = INK_SOFT;
  if (tall) {
    ctx.textAlign = "center";
    ctx.fillText(d.tagline, w / 2, qrY - 56);
    ctx.textAlign = "left";
  } else {
    // Wrapped: the wide card leaves the tagline about half its width once the
    // QR has taken the right-hand column.
    const lines = wrap(ctx, d.tagline, w - pad * 2 - qrSide - 56, 2);
    lines.forEach((line, i) => {
      ctx.fillText(line, pad, h - pad - (lines.length - 1 - i) * (taglineSize * 1.45));
    });
  }
}

/** The card as a PNG blob, ready to download or hand to `navigator.share`. */
export function cardBlob(canvas: HTMLCanvasElement): Promise<Blob | null> {
  return new Promise((resolve) => canvas.toBlob((b) => resolve(b), "image/png"));
}

/** A filename that says what it is when it lands in a Downloads folder. */
export function cardFileName(shape: CardShape, code: string): string {
  return `asale-${code ? `${code.toLowerCase()}-` : ""}${shape}.png`;
}

// ── drawing primitives ────────────────────────────────────────────────

function background(ctx: CanvasRenderingContext2D, w: number, h: number): void {
  const base = ctx.createLinearGradient(0, 0, w * 0.4, h);
  base.addColorStop(0, "#1b1a3a");
  base.addColorStop(1, "#0d0d1c");
  ctx.fillStyle = base;
  ctx.fillRect(0, 0, w, h);

  // One accent bloom in the top-right corner — the same indigo as the site's
  // `--accent`, lifted enough to read against the near-black.
  const glow = ctx.createRadialGradient(w * 0.86, -h * 0.1, 0, w * 0.86, -h * 0.1, w * 0.72);
  glow.addColorStop(0, "rgba(94,92,204,0.5)");
  glow.addColorStop(1, "rgba(94,92,204,0)");
  ctx.fillStyle = glow;
  ctx.fillRect(0, 0, w, h);

  // The mark again, oversized and nearly invisible, bleeding off the bottom
  // edge. It gives the flat panel something to be, and survives the
  // re-compression every feed puts a shared PNG through.
  ctx.save();
  ctx.globalAlpha = 0.045;
  drawMark(ctx, w * 0.52, h * 0.42, h * 0.95, "#ffffff");
  ctx.restore();

  // Hairline inset, so the card still has an edge when a feed renders it on a
  // dark background of its own.
  ctx.strokeStyle = "rgba(255,255,255,0.08)";
  ctx.lineWidth = 2;
  ctx.strokeRect(1, 1, w - 2, h - 2);
}

function drawMark(ctx: CanvasRenderingContext2D, x: number, y: number, height: number, fill: string): void {
  const s = height / MARK_BOX.h;
  ctx.save();
  ctx.translate(x, y);
  ctx.scale(s, s);
  ctx.translate(-MARK_BOX.x, -MARK_BOX.y);
  ctx.translate(MARK_GROUP.tx, MARK_GROUP.ty);
  ctx.scale(MARK_GROUP.s, MARK_GROUP.s);
  ctx.fillStyle = fill;
  for (const d of MARK_PATHS) ctx.fill(new Path2D(d));
  ctx.restore();
}

/** The period chip, right-aligned on `xRight`. */
function pill(ctx: CanvasRenderingContext2D, xRight: number, yMid: number, text: string, size: number): void {
  ctx.font = `600 ${size}px ${SANS}`;
  const padX = size * 0.8;
  const w = ctx.measureText(text).width + padX * 2;
  const h = size * 2;
  rrect(ctx, xRight - w, yMid - h / 2, w, h, h / 2);
  ctx.fillStyle = "rgba(255,255,255,0.1)";
  ctx.fill();
  ctx.strokeStyle = "rgba(255,255,255,0.14)";
  ctx.lineWidth = 1;
  ctx.stroke();
  ctx.fillStyle = INK;
  ctx.textBaseline = "middle";
  ctx.textAlign = "center";
  ctx.fillText(text, xRight - w / 2, yMid + 1);
  ctx.textAlign = "left";
}

/**
 * The invite link as a QR, on a white plate.
 *
 * White, not the card's own dark: a code inverted against a dark plate is
 * within spec and half the phone cameras in the world still refuse it. The
 * plate carries the 4-module quiet zone the spec requires — a code butted
 * against its container is the classic "won't scan".
 */
function drawQr(ctx: CanvasRenderingContext2D, payload: string, x: number, y: number, side: number): void {
  let qr;
  try {
    qr = QRCode.create(payload, { errorCorrectionLevel: "M" }).modules;
  } catch {
    // Only reachable past QR capacity, which an invite link never is.
    return;
  }
  rrect(ctx, x, y, side, side, 20);
  ctx.fillStyle = "#ffffff";
  ctx.fill();

  const quiet = 4;
  const span = qr.size + quiet * 2;
  const m = side / span;
  ctx.fillStyle = "#0d0d1c";
  for (let r = 0; r < qr.size; r++) {
    for (let c = 0; c < qr.size; c++) {
      if (!qr.data[r * qr.size + c]) continue;
      // +0.5 on the span, floor on the origin: adjacent modules must share an
      // exact edge, or the code gets hairline gaps that scanners read as noise.
      const px = Math.floor(x + (c + quiet) * m);
      const py = Math.floor(y + (r + quiet) * m);
      ctx.fillRect(px, py, Math.ceil(m) + 0.5, Math.ceil(m) + 0.5);
    }
  }
}

function rrect(ctx: CanvasRenderingContext2D, x: number, y: number, w: number, h: number, r: number): void {
  ctx.beginPath();
  // `roundRect` is everywhere this app runs, but the fallback costs four lines
  // and saves a blank card on an older WebView.
  if (typeof ctx.roundRect === "function") {
    ctx.roundRect(x, y, w, h, r);
    return;
  }
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + h, r);
  ctx.arcTo(x + w, y + h, x, y + h, r);
  ctx.arcTo(x, y + h, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
  ctx.closePath();
}

/** Largest size at or below `start` at which `text` fits `maxWidth`. Earnings
 *  run from "0.01" to six figures, and the headline must not run under the QR
 *  at the top of that range. */
function fitText(
  ctx: CanvasRenderingContext2D,
  text: string,
  maxWidth: number,
  start: number,
  weight: string,
): number {
  let size = start;
  while (size > 32) {
    ctx.font = `${weight} ${size}px ${SANS}`;
    if (ctx.measureText(text).width <= maxWidth) break;
    size -= 4;
  }
  return size;
}

/**
 * Greedy wrap to at most `maxLines`, clipping the overflow.
 *
 * An atom is a Latin word *or a single CJK character*: a Chinese tagline has no
 * spaces in it, so a word-only wrapper would emit one line and let it run off
 * the edge. Lines are stored with their original trailing space and trimmed on
 * the way out, so folding the overflow back together at the end reconstructs
 * the spacing instead of gluing two English words into one.
 */
function wrap(ctx: CanvasRenderingContext2D, text: string, maxWidth: number, maxLines: number): string[] {
  const atoms = text.match(/[A-Za-z0-9@._/-]+\s*|\S|\s/g) ?? [text];
  const all: string[] = [];
  let line = "";
  for (const atom of atoms) {
    const next = line + atom;
    if (line && ctx.measureText(next.trimEnd()).width > maxWidth) {
      all.push(line);
      line = atom.replace(/^\s+/, "");
    } else {
      line = next;
    }
  }
  if (line.trim()) all.push(line);
  if (all.length <= maxLines) return all.map((l) => l.trimEnd());

  // Past the cap everything is folded back onto the last visible line and cut
  // with an ellipsis — dropping it silently would end the sentence mid-word
  // with nothing to show that anything was removed.
  let tail = all.slice(maxLines - 1).join("").trim();
  while (tail.length > 1 && ctx.measureText(`${tail}…`).width > maxWidth) tail = tail.slice(0, -1);
  return [...all.slice(0, maxLines - 1).map((l) => l.trimEnd()), `${tail}…`];
}
