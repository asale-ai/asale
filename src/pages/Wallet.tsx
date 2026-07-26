import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import QRCode from "qrcode";
import {
  invoke, inTauri, fmtUsdt,
  type Wallet, type WalletHistory, type WithdrawLimits,
} from "../lib";
import { Card, CopyChip, Err, Ok, Skeleton, useCopy, PageHead, IconAction, Empty, FactGrid } from "../ui";
import {
  IconWallet, IconRefresh, IconDownload, IconArrowRight,
  IconShield, IconCheck, IconCopy, IconRecords, IconAlert,
} from "../icons";

const TRON_RE = /^T[1-9A-HJ-NP-Za-km-z]{33}$/;

type Pane = "deposit" | "withdraw";
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

  const [pane, setPane] = useState<Pane>("deposit");
  const [histTab, setHistTab] = useState<HistTab>("all");

  const [addr, setAddr] = useState("");
  const [qr, setQr] = useState("");
  const [addrBusy, setAddrBusy] = useState(false);
  const [addrErr, setAddrErr] = useState("");

  const [to, setTo] = useState("");
  const [amount, setAmount] = useState("");
  const [wdBusy, setWdBusy] = useState(false);
  const [wdErr, setWdErr] = useState("");
  const [wdOk, setWdOk] = useState("");

  const [copiedHash, copyHash] = useCopy();

  const refresh = useCallback((manual = false) => {
    if (!inTauri) return;
    if (manual) setRefreshing(true);
    Promise.allSettled([
      invoke<Wallet>("wallet_overview").then((v) => { setW(v); setErr(""); })
        .catch((e) => setErr(String((e as Error).message))),
      // History is best-effort: an older server without the endpoint must not
      // take the balance view down with it.
      invoke<WalletHistory>("wallet_history").then(setHist).catch(() => {}),
    ]).finally(() => { setLoading(false); setRefreshing(false); });
  }, []);
  useEffect(() => { refresh(); }, [refresh]);

  async function getAddress() {
    setAddrBusy(true); setAddrErr("");
    try {
      const r = await invoke<{ chain: string; address: string }>("wallet_deposit_address", { chain: "tron" });
      setAddr(r.address);
      setQr(await QRCode.toDataURL(r.address, { margin: 1, width: 220 }));
    } catch (e) { setAddrErr(String((e as Error).message)); } finally { setAddrBusy(false); }
  }

  const limits: WithdrawLimits | null = hist?.limits ?? null;
  const availableMicros = w?.balance ?? 0;
  const amountMicros = Math.round(parseFloat(amount || "0") * 1_000_000);
  const addrValid = TRON_RE.test(to.trim());
  const minMicros = limits?.withdraw_min ?? 0;
  const maxSingle = limits?.withdraw_max_single ?? 0;
  const belowMin = amountMicros > 0 && minMicros > 0 && amountMicros < minMicros;
  const overSingle = maxSingle > 0 && amountMicros > maxSingle;
  const amountValid =
    Number.isFinite(amountMicros) && amountMicros > 0 &&
    amountMicros <= availableMicros && !belowMin && !overSingle;

  async function withdraw() {
    setWdErr(""); setWdOk("");
    if (!addrValid) return setWdErr(t("wallet.invalidAddress"));
    if (belowMin) return setWdErr(t("wallet.belowMin", { amount: fmtUsdt(minMicros) }));
    if (overSingle) return setWdErr(t("wallet.overSingle", { amount: fmtUsdt(maxSingle) }));
    if (!amountValid) return setWdErr(t("wallet.invalidAmount"));
    setWdBusy(true);
    try {
      const r = await invoke<{ withdrawal_id: number; status: string }>("wallet_withdraw", {
        chain: "tron", toAddress: to.trim(), amount: amountMicros,
      });
      setWdOk(t("wallet.withdrawOk", { id: r.withdrawal_id }));
      setTo(""); setAmount(""); refresh();
    } catch (e) { setWdErr(String((e as Error).message)); } finally { setWdBusy(false); }
  }

  // Deposits and withdrawals merged into one time-ordered feed.
  const flows = useMemo<Flow[]>(() => {
    const d: Flow[] = (hist?.deposits ?? []).map((r) => ({
      key: `d${r.id}`, kind: "deposit", ts: r.created_ts, amount: r.amount,
      fee: 0, status: r.status, hash: r.tx_hash, target: null,
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

  const panes: { id: Pane; label: string }[] = [
    { id: "deposit", label: t("wallet.tabDeposit") },
    { id: "withdraw", label: t("wallet.tabWithdraw") },
  ];
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

      {err && <div className="callout danger" style={{ marginBottom: 16 }}><IconWallet /><span>{err} — {t("wallet.signInFirst")}</span></div>}

      {/* ── Balance hero ── */}
      <div className="wallet-hero">
        <div className="wh-main">
          <div className="wh-label">{t("wallet.available")}</div>
          <div className="wh-amount mono tabular">
            {loading ? <Skeleton w={210} h={44} r={12} /> : (
              <>{w ? fmtUsdt(availableMicros) : "—"}<span className="wh-unit">USDT</span></>
            )}
          </div>
          <div className="wh-trust">
            <span className="trust-chip"><IconShield />{t("wallet.trustCustody")}</span>
            <span className="trust-chip"><IconCheck />{t("wallet.trustNetwork")}</span>
            <span className="trust-chip"><IconRecords />{t("wallet.trustAudit")}</span>
          </div>
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

      {/* ── Deposit / withdraw ── */}
      <Card>
        <div className="segmented" style={{ marginBottom: 18 }}>
          {panes.map((p) => (
            <button key={p.id} className={pane === p.id ? "active" : ""} onClick={() => setPane(p.id)}>
              {p.id === "deposit" ? <IconDownload /> : <IconArrowRight />}{p.label}
            </button>
          ))}
        </div>

        {pane === "deposit" ? (
          <div className="fade-in">
            <FactGrid facts={[
              { k: t("wallet.factNetwork"), v: "TRON · TRC20" },
              { k: t("wallet.factAsset"), v: "USDT" },
              { k: t("wallet.factCredit"), v: t("wallet.factCreditVal") },
            ]} />

            {!addr && (
              <div className="wallet-cta">
                <p className="card-desc" style={{ margin: "0 0 14px" }}>{t("wallet.depositNote")}</p>
                <button className="btn" onClick={getAddress} disabled={!inTauri || addrBusy}>
                  {addrBusy ? <IconRefresh className="spin" /> : <IconDownload />}
                  {addrBusy ? t("wallet.gettingAddress") : t("wallet.getAddress")}
                </button>
                {addrErr && <Err>{addrErr}</Err>}
              </div>
            )}

            {addr && (
              <div className="deposit-panel fade-in">
                <div className="qr-box">
                  {qr && <img src={qr} alt="deposit address QR" width={168} height={168} />}
                  <span className="qr-cap">{t("wallet.scanToPay")}</span>
                </div>
                <div className="deposit-side">
                  <div className="field" style={{ marginBottom: 12 }}>
                    <label>{t("wallet.yourAddress")}</label>
                    <CopyChip value={addr} wrap />
                  </div>
                  <div className="callout warn">
                    <IconAlert /><span>{t("wallet.trc20Only")}</span>
                  </div>
                </div>
              </div>
            )}
          </div>
        ) : (
          <div className="fade-in">
            <FactGrid facts={[
              { k: t("wallet.factMin"), v: <span className="mono">{limits ? `${fmtUsdt(limits.withdraw_min)} USDT` : "—"}</span> },
              { k: t("wallet.factMaxSingle"), v: <span className="mono">{limits ? `${fmtUsdt(limits.withdraw_max_single)} USDT` : "—"}</span> },
              { k: t("wallet.factMaxDaily"), v: <span className="mono">{limits ? `${fmtUsdt(limits.withdraw_max_daily)} USDT` : "—"}</span> },
            ]} />

            <div className="field">
              <label>{t("wallet.withdrawAddress")}</label>
              <input className={`input mono ${to.trim() && !addrValid ? "invalid" : ""}`} value={to}
                onChange={(e) => setTo(e.target.value)} placeholder="T…" spellCheck={false} />
              <div className="hint" style={to.trim() && !addrValid ? { color: "var(--danger)" } : undefined}>
                {to.trim() && !addrValid ? t("wallet.invalidAddress") : t("wallet.addressHint")}
              </div>
            </div>

            <div className="field">
              <label>{t("wallet.withdrawAmount")}</label>
              <div style={{ display: "flex", gap: 8 }}>
                <input className={`input mono ${amount && !amountValid ? "invalid" : ""}`} value={amount}
                  onChange={(e) => setAmount(e.target.value)} placeholder="0.00" inputMode="decimal" />
                <button className="btn ghost" onClick={() => setAmount((availableMicros / 1_000_000).toString())}
                  disabled={!w || availableMicros <= 0}>{t("wallet.max")}</button>
              </div>
              <div className="hint">{t("wallet.availableHint", { amount: w ? fmtUsdt(availableMicros) : "—" })}</div>
            </div>

            {/* What actually happens on submit, spelled out before the button. */}
            <div className="wd-summary">
              <div className="wd-line">
                <span>{t("wallet.sumAmount")}</span>
                <span className="mono tabular">{amountMicros > 0 ? fmtUsdt(amountMicros) : "0.00"} USDT</span>
              </div>
              <div className="wd-line">
                <span>{t("wallet.sumNetwork")}</span>
                <span>TRON · TRC20</span>
              </div>
              <div className="wd-line strong">
                <span>{t("wallet.sumFlow")}</span>
                <span>{t("wallet.sumFlowVal")}</span>
              </div>
            </div>

            {limits?.whitelist_only && (
              <div className="callout warn" style={{ marginBottom: 14 }}>
                <IconShield /><span>{t("wallet.whitelistOnly")}</span>
              </div>
            )}

            <button className="btn block lg" onClick={withdraw} disabled={!inTauri || wdBusy || !addrValid || !amountValid}>
              {wdBusy ? <IconRefresh className="spin" /> : <IconArrowRight />}
              {wdBusy ? t("wallet.withdrawing") : t("wallet.withdrawSubmit")}
            </button>
            <Err>{wdErr}</Err>
            <Ok>{wdOk}</Ok>
          </div>
        )}
      </Card>

      {/* ── Deposit / withdrawal history ── */}
      <Card>
        <div style={{ display: "flex", alignItems: "center", gap: 12, flexWrap: "wrap", marginBottom: 14 }}>
          <h3 style={{ flex: 1 }}>{t("wallet.historyTitle")}</h3>
          <div className="tabstrip">
            {histTabs.map((h) => (
              <button key={h.id} className={histTab === h.id ? "active" : ""} onClick={() => setHistTab(h.id)}>{h.label}</button>
            ))}
          </div>
        </div>

        {loading ? (
          <div className="stack-gap">
            {Array.from({ length: 3 }, (_, i) => <Skeleton key={i} h={44} r={10} />)}
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
                  <th style={{ textAlign: "right" }}>{t("wallet.colAmount")}</th>
                  <th>{t("wallet.colStatus")}</th>
                  <th>{t("wallet.colTx")}</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((f) => (
                  <tr key={f.key}>
                    <td className="tabular" style={{ whiteSpace: "nowrap" }}>{fmtTime(f.ts)}</td>
                    <td>
                      <span className={`flow-kind ${f.kind}`}>
                        {f.kind === "deposit" ? <IconDownload /> : <IconArrowRight />}
                        {t(f.kind === "deposit" ? "wallet.kindDeposit" : "wallet.kindWithdraw")}
                      </span>
                    </td>
                    <td className="mono tabular" style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                      <span className={f.kind === "deposit" ? "amt-in" : "amt-out"}>
                        {f.kind === "deposit" ? "+" : "−"}{fmtUsdt(f.amount)}
                      </span>
                      {f.fee > 0 && <div className="faint" style={{ fontSize: 11 }}>{t("wallet.feeN", { amount: fmtUsdt(f.fee) })}</div>}
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
