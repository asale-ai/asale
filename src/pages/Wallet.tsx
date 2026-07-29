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
import { Card, Skeleton, useCopy, PageHead, IconAction, Empty } from "../ui";
import { WalletDialog, type WalletMode } from "../components/WalletDialog";
import { errText } from "../errors";
import {
  IconWallet, IconRefresh, IconDownload, IconArrowRight,
  IconShield, IconCheck, IconCopy, IconRecords,
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
  hash: string | null;
  target: string | null;
}

const fmtTime = (secs: number) => (secs ? new Date(secs * 1000).toLocaleString() : "—");
const shorten = (s: string, head = 8, tail = 6) =>
  s.length <= head + tail + 1 ? s : `${s.slice(0, head)}…${s.slice(-tail)}`;

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
  const { t } = useTranslation();
  const [w, setW] = useState<Wallet | null>(null);
  const [hist, setHist] = useState<WalletHistory | null>(null);
  const [loading, setLoading] = useState(inTauri);
  const [refreshing, setRefreshing] = useState(false);
  const [err, setErr] = useState("");

  /* Which funding sheet is open; null = none. */
  const [pane, setPane] = useState<WalletMode | null>(null);
  const [histTab, setHistTab] = useState<HistTab>("all");

  const [copiedHash, copyHash] = useCopy();

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
      fee: r.fee ?? 0, status: r.status, hash: r.tx_hash, target: null,
    }));
    const x: Flow[] = (hist?.withdrawals ?? []).map((r) => ({
      key: `w${r.id}`, kind: "withdraw", ts: r.confirmed_ts || r.requested_ts, amount: r.amount,
      fee: r.fee ?? 0, status: r.status, hash: r.tx_hash, target: r.to_address,
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
            <button className="btn" onClick={() => setPane("deposit")} disabled={!inTauri}>
              <IconDownload />{t("wallet.tabDeposit")}
            </button>
            <button className="btn ghost" onClick={() => setPane("withdraw")} disabled={!inTauri}>
              <IconArrowRight />{t("wallet.tabWithdraw")}
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
                  <th>{t("wallet.colTx")}</th>
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
                    <td>
                      {f.hash ? (
                        <button className="hash-btn mono" onClick={() => copyHash(f.hash!)} title={f.hash}>
                          {copiedHash ? <IconCheck /> : <IconCopy />}{shorten(f.hash)}
                        </button>
                      ) : f.target ? (
                        <span className="mono faint" title={f.target}>{shorten(f.target, 6, 4)}</span>
                      ) : <span className="faint">—</span>}
                    </td>
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
