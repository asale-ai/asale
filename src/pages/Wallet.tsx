// The page answers one question — what do you have — and the two verbs hang off
// that figure. Funding used to live in a second card below it with a segmented
// switch; both rails now open in `WalletDialog`, which keeps the page to a
// balance and a history, and gives new funding methods somewhere to land that
// is not "another tab on the wallet screen".
import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri, fmtUsdt,
  type Wallet, type WalletHistory,
} from "../lib";
import { Card, Skeleton, PageHead, IconAction, Empty } from "../ui";
import { WalletDialog, CardTopUpDialog, type WalletMode } from "../components/WalletDialog";
import { EarningsShareDialog } from "../components/EarningsShareDialog";
import { errText } from "../errors";
import { sitePage } from "../links";
import { openExternal } from "../shell";
import {
  IconWallet, IconRefresh, IconDownload, IconArrowRight,
  IconShield, IconCheck, IconRecords, IconShare, IconExternal,
} from "../icons";

type HistTab = "all" | "deposit" | "withdraw";

/** One row of the unified money-in/money-out history. */
interface Flow {
  key: string;
  kind: "deposit" | "withdraw";
  ts: number;
  amount: number;
  fee: number;
  status: number;
}

const fmtTime = (secs: number) => (secs ? new Date(secs * 1000).toLocaleString() : "—");

/** Deposit status → pill tone + i18n key. 1 seen · 2 confirmed · 3 credited. */
const DEPOSIT_STATUS: Record<number, [string, string]> = {
  1: ["warn", "depSeen"], 2: ["warn", "depConfirmed"], 3: ["on", "depCredited"],
};
/** Withdrawal status. 1 requested · 2 risk_review · 3 broadcast · 4 confirmed · 5 rejected. */
const WITHDRAW_STATUS: Record<number, [string, string]> = {
  1: ["warn", "wdRequested"], 2: ["warn", "wdReview"], 3: ["warn", "wdBroadcast"],
  4: ["on", "wdConfirmed"], 5: ["err", "wdRejected"],
};

export function WalletPage() {
  const { t, i18n } = useTranslation();
  const [w, setW] = useState<Wallet | null>(null);
  const [hist, setHist] = useState<WalletHistory | null>(null);
  const [loading, setLoading] = useState(inTauri);
  const [refreshing, setRefreshing] = useState(false);
  const [err, setErr] = useState("");

  /* Which funding sheet is open; null = none. */
  const [pane, setPane] = useState<WalletMode | null>(null);
  const [sharing, setSharing] = useState(false);
  /* The card sheet is its own door, not a tab on the one above. */
  const [cardOpen, setCardOpen] = useState(false);
  const [histTab, setHistTab] = useState<HistTab>("all");

  const refresh = useCallback((manual = false) => {
    if (!inTauri) return;
    if (manual) setRefreshing(true);
    Promise.allSettled([
      invoke<Wallet>("wallet_overview").then((v) => { setW(v); setErr(""); })
        .catch((e) => setErr(errText(e))),
      // History is best-effort: an older server without the endpoint must not
      // take the balance view down with it.
      invoke<WalletHistory>("wallet_history").then(setHist).catch(() => {}),
    ]).finally(() => { setLoading(false); setRefreshing(false); });
  }, []);
  useEffect(() => { refresh(); }, [refresh]);

  const availableMicros = w?.balance ?? 0;

  // Deposits and withdrawals merged into one time-ordered feed.
  const flows = useMemo<Flow[]>(() => {
    const d: Flow[] = (hist?.deposits ?? []).map((r) => ({
      key: `d${r.id}`, kind: "deposit", ts: r.created_ts, amount: r.amount,
      fee: r.fee ?? 0, status: r.status,
    }));
    const x: Flow[] = (hist?.withdrawals ?? []).map((r) => ({
      key: `w${r.id}`, kind: "withdraw", ts: r.confirmed_ts || r.requested_ts, amount: r.amount,
      fee: r.fee ?? 0, status: r.status,
    }));
    return [...d, ...x].sort((a, b) => b.ts - a.ts);
  }, [hist]);
  const rows = flows.filter((f) => histTab === "all" || f.kind === histTab);

  const statusPill = (f: Flow) => {
    const map = f.kind === "deposit" ? DEPOSIT_STATUS : WITHDRAW_STATUS;
    const [cls, key] = map[f.status] ?? ["off", "unknownStatus"];
    return <span className={`pill ${cls}`}>{t(`wallet.${key}`)}</span>;
  };

  const histTabs: { id: HistTab; label: string }[] = [
    { id: "all", label: t("wallet.histAll") },
    { id: "deposit", label: t("wallet.histDeposit") },
    { id: "withdraw", label: t("wallet.histWithdraw") },
  ];

  return (
    <div>
      <PageHead
        title={t("wallet.title")}
        sub={t("wallet.sub")}
        actions={
          <IconAction
            icon={<IconRefresh />}
            label={t("wallet.refresh")}
            onClick={() => refresh(true)}
            disabled={!inTauri || refreshing}
            spinning={refreshing}
          />
        }
      />

      {err && <div className="callout danger card-lead"><IconWallet /><span>{err} — {t("wallet.signInFirst")}</span></div>}

      {/* ── Balance hero ── */}
      <div className="wallet-hero">
        <div className="wh-main">
          <div className="wh-label">{t("wallet.available")}</div>
          <div className="wh-amount mono tabular">
            {loading ? <Skeleton w={210} h={44} r={12} /> : (
              <>{w ? fmtUsdt(availableMicros) : "—"}<span className="wh-unit">USDT</span></>
            )}
          </div>
          <div className="wh-actions">
            {/* Two rails, two buttons. The crypto one used to live in a dropdown
                behind a chevron on the card button, which is the wrong shape for
                this audience: the people who already hold USDT are the ones who
                can pay in one step, and they were the ones being asked to go
                looking. Neither rail is a sub-case of the other, so neither is
                hidden inside the other.

                The card stays the primary because it is the only rail someone
                can finish without already holding USDT. Where no processor is
                configured, the crypto button is the only one and takes the
                primary styling itself. Kept in step with asale-web's wallet. */}
            {hist?.card ? (
              <>
                <button className="btn" onClick={() => setCardOpen(true)} disabled={!inTauri}>
                  <IconDownload />{t("wallet.depositCard")}
                </button>
                <button className="btn ghost" onClick={() => setPane("deposit")} disabled={!inTauri}>
                  <IconWallet />{t("wallet.depositCrypto")}
                </button>
              </>
            ) : (
              <button className="btn" onClick={() => setPane("deposit")} disabled={!inTauri}>
                <IconDownload />{t("wallet.tabDeposit")}
              </button>
            )}
            <button className="btn ghost" onClick={() => setPane("withdraw")} disabled={!inTauri}>
              <IconArrowRight />{t("wallet.tabWithdraw")}
            </button>
            {/* Beside the two money verbs, because this is where the number
                being shared is already on screen. */}
            <button className="btn ghost" onClick={() => setSharing(true)} disabled={!inTauri}>
              <IconShare />{t("share.open")}
            </button>
          </div>
          <div className="wh-trust">
            <span className="trust-chip"><IconShield />{t("wallet.trustCustody")}</span>
            <span className="trust-chip"><IconCheck />{t("wallet.trustNetwork")}</span>
            <span className="trust-chip"><IconRecords />{t("wallet.trustAudit")}</span>
          </div>
          <WalletDialog
            mode={pane}
            limits={hist}
            balance={availableMicros}
            onClose={() => setPane(null)}
            onDone={() => refresh()}
          />
          <CardTopUpDialog
            open={cardOpen}
            limits={hist?.card ?? null}
            onClose={() => setCardOpen(false)}
            onDone={() => refresh()}
          />
          {sharing && <EarningsShareDialog onClose={() => setSharing(false)} />}
        </div>
        <div className="wh-side">
          <div className="wh-cell">
            <span className="wh-k">{t("wallet.frozen")}</span>
            <span className="wh-v mono tabular">{loading ? <Skeleton w={64} h={16} /> : w ? fmtUsdt(w.frozen) : "—"}</span>
          </div>
          <div className="wh-cell">
            <span className="wh-k">{t("wallet.payable")}</span>
            <span className="wh-v mono tabular">{loading ? <Skeleton w={64} h={16} /> : w ? fmtUsdt(w.payable) : "—"}</span>
          </div>
        </div>
      </div>

      {/* ── Invite & earn ──
          A way out to the website, not a page of its own here: the link, the
          code, the invitee list and the settlement rules all live there, and a
          second copy in the desktop shell would be one more thing to keep
          truthful. It sits under the balance because commission lands in that
          balance. */}
      <button
        type="button"
        className="linkrow"
        onClick={() => openExternal(sitePage("referral", i18n.language))}
      >
        <span className="lr-ico"><IconShare /></span>
        <span className="lr-body">
          <span className="lr-t">{t("wallet.referralTitle")}</span>
          <span className="lr-d">{t("wallet.referralDesc")}</span>
        </span>
        <span className="lr-go"><IconExternal /></span>
      </button>

      {/* ── Deposit / withdrawal history ── */}
      <Card>
        <div className="card-toolbar">
          <h3>{t("wallet.historyTitle")}</h3>
          <div className="tabstrip">
            {histTabs.map((h) => (
              <button key={h.id} className={histTab === h.id ? "active" : ""} onClick={() => setHistTab(h.id)}>{h.label}</button>
            ))}
          </div>
        </div>

        {loading ? (
          <div className="stack-gap">
            {Array.from({ length: 3 }, (_, i) => <Skeleton key={i} h={40} r={10} />)}
          </div>
        ) : rows.length === 0 ? (
          <Empty icon={<IconRecords />} title={t("wallet.histEmpty")} desc={t("wallet.histEmptyDesc")} />
        ) : (
          <div className="table-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>{t("wallet.colTime")}</th>
                  <th>{t("wallet.colType")}</th>
                  <th className="num">{t("wallet.colAmount")}</th>
                  <th>{t("wallet.colStatus")}</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((f) => (
                  <tr key={f.key}>
                    <td className="tabular nowrap">{fmtTime(f.ts)}</td>
                    <td>
                      <span className={`flow-kind ${f.kind}`}>
                        {f.kind === "deposit" ? <IconDownload /> : <IconArrowRight />}
                        {t(f.kind === "deposit" ? "wallet.kindDeposit" : "wallet.kindWithdraw")}
                      </span>
                    </td>
                    <td className="mono tabular num nowrap">
                      <span className={f.kind === "deposit" ? "amt-in" : "amt-out"}>
                        {f.kind === "deposit" ? "+" : "−"}{fmtUsdt(f.amount)}
                      </span>
                      {f.fee > 0 && <div className="td-note">{t("wallet.feeN", { amount: fmtUsdt(f.fee) })}</div>}
                    </td>
                    <td>{statusPill(f)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </div>
  );
}
