// Preview the earnings card, pick what it says and what shape it is, send it
// somewhere.
//
// KEEP IN SYNC with asale-web/src/components/EarningsShareDialog.tsx — with one
// deliberate difference: this app has two things worth putting on a card and
// the web has one. The desktop client is where selling happens, so that is
// where the sell card is offered; the web console only ever renders the
// referral side.
//
// The sell figures are the settled ledger's (`usage_summary` asks the server
// for them). They cannot be taken off the daemon's own record of what it
// served: that record is written when the call is served, before anything has
// been priced, so its amount is 0 and a card built from it announced earnings
// of 0.00 next to millions of shared tokens.
//
// The preview is the real canvas at full resolution, scaled down by CSS rather
// than drawn small — what is on screen is byte-for-byte what gets shared, so
// the two cannot drift apart.

import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { fmtUsdt, fmtTokens, invoke, type UsageSummary } from "../lib";
import {
  cardBlob,
  cardFileName,
  drawEarningsCard,
  type CardData,
  type CardShape,
} from "../lib/earnings-card";
import type { SharePayload } from "../lib/share";
import { ShareSheet } from "./ShareSheet";
import { IconX } from "../icons";

/** What the headline number is. */
type Mode = "sell" | "referral";

/** Periods the sell card can report, in the daemon's own vocabulary. */
const PERIODS = ["day", "week", "month", "all"] as const;
type Period = (typeof PERIODS)[number];

const SHAPES: CardShape[] = ["wide", "tall"];

/** The subset of `/me/referral` this dialog reads. Mirrors `ReferralOverview`
 *  in asale-web/src/lib/api.ts; the daemon passes the server's reply through
 *  untouched. */
interface Referral {
  enabled: boolean;
  code: string;
  link: string;
  signup_bonus: number;
  stats: { invited: number; activated: number; pending: number; paid: number };
  unit: string;
}

export function EarningsShareDialog({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const [shape, setShape] = useState<CardShape>("wide");
  const [mode, setMode] = useState<Mode>("sell");
  const [period, setPeriod] = useState<Period>("month");
  const [file, setFile] = useState<File | null>(null);
  const [sold, setSold] = useState<UsageSummary | null>(null);
  const [referral, setReferral] = useState<Referral | null>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);

  // The invite link is what makes the card worth sharing, and it is the same
  // for both modes — fetched once, not per mode. A failure is not fatal: the
  // card simply draws without a QR, which is better than refusing to open.
  useEffect(() => {
    invoke<Referral>("me_referral").then(setReferral).catch(() => {});
  }, []);

  useEffect(() => {
    if (mode !== "sell") return;
    let live = true;
    invoke<UsageSummary>("usage_summary", { period })
      .then((s) => live && setSold(s))
      .catch(() => live && setSold(null));
    return () => {
      live = false;
    };
  }, [mode, period]);

  // Nothing to sell yet and no referrals either is a normal state for a fresh
  // install; the card still renders, with zeros, and the invite link is the
  // part that matters.
  const card: CardData = useMemo(() => {
    const link = referral?.link ?? "";
    const common = {
      tagline: t("share.card.tagline"),
      link,
      code: referral?.code ?? "",
      codeHint: t("share.card.codeHint"),
      site: t("share.card.site"),
    };
    if (mode === "referral") {
      const stats = referral?.stats;
      return {
        ...common,
        kicker: t("share.card.kickerReferral"),
        // Paid *plus* pending: commission is credited when a referee trades and
        // released later, so "already earned" is the sum. Paid alone reports a
        // week-old invite as zero.
        amount: fmtUsdt((stats?.paid ?? 0) + (stats?.pending ?? 0)),
        // Wire unit is micro_usdt; `fmtUsdt` already converted to display USDT.
        unit: "USDT",
        period: t("share.card.periodAll"),
        stats: [
          { label: t("share.card.statInvited"), value: String(stats?.invited ?? 0) },
          { label: t("share.card.statActivated"), value: String(stats?.activated ?? 0) },
          { label: t("share.card.statPending"), value: fmtUsdt(stats?.pending ?? 0) },
        ],
      };
    }
    return {
      ...common,
      kicker: t("share.card.kickerSell"),
      amount: fmtUsdt(sold?.sold.amount_usdt ?? 0),
      unit: "USDT",
      period: t(`share.period_${period}`),
      stats: [
        { label: t("share.card.statTokens"), value: fmtTokens(sold?.sold.tokens ?? 0) },
        { label: t("share.card.statCalls"), value: String(sold?.sold.count ?? 0) },
      ],
    };
  }, [mode, period, sold, referral, t]);

  const payload: SharePayload = useMemo(() => {
    const url = referral?.link || "https://asale.ai/";
    return {
      url,
      title: t("share.text.title"),
      text:
        mode === "sell" && (sold?.sold.amount_usdt ?? 0) > 0
          ? t("share.text.bodySell", {
              period: t(`share.period_${period}`),
              amount: fmtUsdt(sold?.sold.amount_usdt ?? 0),
            })
          : t("share.text.body", { bonus: fmtUsdt(referral?.signup_bonus ?? 0) }),
    };
  }, [mode, period, sold, referral, t]);

  const fileName = useMemo(() => cardFileName(shape, card.code), [shape, card.code]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    drawEarningsCard(canvas, card, shape);
    // The blob is async, so the share buttons that need a file stay disabled
    // for a frame. `live` drops the result of a card the reader has already
    // switched away from — otherwise a fast toggle can leave last card's file
    // attached to this one.
    let live = true;
    setFile(null);
    cardBlob(canvas).then((blob) => {
      if (live && blob) setFile(new File([blob], fileName, { type: "image/png" }));
    });
    return () => {
      live = false;
    };
  }, [card, shape, fileName]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="modal-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="modal wdlg wide" role="dialog" aria-modal="true" aria-label={t("share.dialogTitle")}>
        <div className="modal-head">
          <h3>{t("share.dialogTitle")}</h3>
          <button type="button" className="modal-x" onClick={onClose} title={t("share.close")}>
            <IconX />
          </button>
        </div>

        <div className="wdlg-tabs ec-tabs">
          <div className="segmented">
            <button className={mode === "sell" ? "active" : ""} onClick={() => setMode("sell")}>
              {t("share.modeSell")}
            </button>
            {/* Hidden rather than disabled when the programme is switched off
                server-side: a card whose headline is a commission nobody can
                earn is not worth explaining. */}
            {referral?.enabled !== false && (
              <button className={mode === "referral" ? "active" : ""} onClick={() => setMode("referral")}>
                {t("share.modeReferral")}
              </button>
            )}
          </div>
          {mode === "sell" && (
            <div className="segmented sm">
              {PERIODS.map((p) => (
                <button key={p} className={p === period ? "active" : ""} onClick={() => setPeriod(p)}>
                  {t(`share.period_${p}`)}
                </button>
              ))}
            </div>
          )}
          <div className="segmented sm">
            {SHAPES.map((s) => (
              <button key={s} className={s === shape ? "active" : ""} onClick={() => setShape(s)}>
                {t(`share.shape_${s}`)}
              </button>
            ))}
          </div>
        </div>

        <div className="wdlg-body">
          <div className={`ec-preview ${shape}`}>
            <canvas ref={canvasRef} aria-label={t("share.cardAlt")} role="img" />
          </div>
          <p className="ec-hint">{t(`share.shapeHint_${shape}`)}</p>
          <ShareSheet payload={payload} file={file} fileName={fileName} />
        </div>
      </div>
    </div>
  );
}
