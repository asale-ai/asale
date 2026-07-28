// Money in and money out, as a dialog rather than a second card on the wallet
// page. The page now shows one thing — what you have — and the two verbs hang
// off it; the funding rails live behind them.
//
// One view, not a wizard: the rails are tabs and the selected rail is fully
// rendered straight away — the deposit address and its QR are fetched on open,
// not put behind a button. `METHODS` is the extension point; adding a rail
// means adding an entry there and a branch in the body.
//
// The sheet is a fixed size. Its body scrolls, so switching tabs never resizes
// the dialog under the cursor — the tallest rail sets the height once and the
// rest live inside it.
//
// Kept in sync with asale-web's `components/WalletDialog.tsx` — same two steps,
// same copy keys, same warning. If you change one, change the other.
import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import QRCode from "qrcode";
import { invoke, inTauri, fmtUsdt, type WithdrawLimits } from "../lib";
import { CopyChip, Err, FactGrid, Skeleton } from "../ui";
import {
  IconX, IconArrowRight, IconRefresh, IconInfo, IconShield, IconWallet,
} from "../icons";

const TRON_RE = /^T[1-9A-HJ-NP-Za-km-z]{33}$/;

export type WalletMode = "deposit" | "withdraw";

/** One funding rail. `id` drives both the copy keys and the body branch. */
interface Method {
  id: string;
  modes: WalletMode[];
  icon: React.ReactNode;
}

const METHODS: Method[] = [
  { id: "tron_usdt", modes: ["deposit", "withdraw"], icon: <IconWallet /> },
];

export function WalletDialog({
  mode, limits, balance, onClose, onDone,
}: {
  mode: WalletMode | null;
  limits: WithdrawLimits | null;
  balance: number;
  onClose: () => void;
  /** Something moved — the page should refetch. */
  onDone: () => void;
}) {
  // Escape closes; the page behind must not scroll while the dialog is up.
  useEffect(() => {
    if (!mode) return;
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", onKey);
    const prev = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      window.removeEventListener("keydown", onKey);
      document.body.style.overflow = prev;
    };
  }, [mode, onClose]);

  if (!mode) return null;
  // Keyed on the direction so reopening the dialog starts from the first tab
  // with an empty form. Nothing to reset by hand.
  return createPortal(
    <Sheet key={mode} mode={mode} limits={limits} balance={balance} onClose={onClose} onDone={onDone} />,
    document.body,
  );
}

function Sheet({
  mode, limits, balance, onClose, onDone,
}: {
  mode: WalletMode;
  limits: WithdrawLimits | null;
  balance: number;
  onClose: () => void;
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const available = METHODS.filter((m) => m.modes.includes(mode));
  const [tab, setTab] = useState(available[0]?.id ?? "");
  const picked = available.find((m) => m.id === tab) ?? available[0] ?? null;
  const title = t(mode === "deposit" ? "wallet.tabDeposit" : "wallet.tabWithdraw");

  return (
    <div className="modal-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="modal wdlg" role="dialog" aria-modal="true" aria-label={title}>
        <div className="modal-head">
          <h3>{title}</h3>
          <button type="button" className="modal-x" onClick={onClose} title={t("wallet.dlgClose")}>
            <IconX />
          </button>
        </div>

        {/* What the money is measured against, on every tab. */}
        <div className="wdlg-bal">
          <span>{t("wallet.available")}</span>
          <span className="mono tabular">{fmtUsdt(balance)} USDT</span>
        </div>

        <div className="wdlg-tabs">
          <div className="segmented">
            {available.map((m) => (
              <button key={m.id} className={m.id === tab ? "active" : ""} onClick={() => setTab(m.id)}>
                {m.icon}{t(`wallet.method_${m.id}`)}
              </button>
            ))}
          </div>
        </div>

        {/* Fixed height: the tab strip must not move when the body changes. */}
        <div className="wdlg-body">
          {picked?.id === "tron_usdt" && (mode === "deposit"
            ? <TronDeposit limits={limits} />
            : <TronWithdraw limits={limits} balance={balance} onDone={onDone} onClose={onClose} />)}
        </div>
      </div>
    </div>
  );
}

/* ── TRON · USDT deposit ─────────────────────────────────────────────────── */

function TronDeposit({ limits }: { limits: WithdrawLimits | null }) {
  const { t } = useTranslation();
  const [addr, setAddr] = useState("");
  const [qr, setQr] = useState("");
  const [err, setErr] = useState("");
  /* The address is what this tab is for, so it is requested on open rather than
     hidden behind a button. Starts busy: there is never a state where the tab
     is idle without one. */
  const [busy, setBusy] = useState(inTauri);

  useEffect(() => {
    if (!inTauri) return;
    let cancelled = false;
    invoke<{ chain: string; address: string }>("wallet_deposit_address", { chain: "tron" })
      .then(async (r) => {
        const png = await QRCode.toDataURL(r.address, { margin: 1, width: 180 });
        if (cancelled) return;
        setAddr(r.address);
        setQr(png);
      })
      .catch((e) => { if (!cancelled) setErr(String((e as Error).message)); })
      .finally(() => { if (!cancelled) setBusy(false); });
    return () => { cancelled = true; };
  }, []);

  return (
    <>
      {/* The network to send over, above the address rather than below it — it
          has to be read before the QR is scanned. Informational, not alarming. */}
      <div className="depwarn">
        <div className="depwarn-head">
          <IconInfo />
          <span className="depwarn-title">{t("wallet.depositWarnTitle")}</span>
        </div>
        <p className="depwarn-body">{t("wallet.depositWarnDesc")}</p>
      </div>

      {busy ? (
        <div className="dep-result">
          <Skeleton w={180} h={180} r={12} />
          <Skeleton h={34} r={8} />
        </div>
      ) : err ? (
        <Err>{err}</Err>
      ) : (
        <div className="dep-result fade-in">
          <div className="qr-box">
            {qr && <img src={qr} alt="deposit address QR" width={180} height={180} />}
            <span className="qr-cap">{t("wallet.scanToPay")}</span>
          </div>
          <div className="field">
            <label>{t("wallet.yourAddress")}</label>
            <CopyChip value={addr} wrap />
          </div>
          <div className="callout info">
            <IconInfo /><span>{t("wallet.trc20Only")}</span>
          </div>
        </div>
      )}

      <FactGrid facts={[
        { k: t("wallet.factNetwork"), v: "TRON · TRC20" },
        { k: t("wallet.factAsset"), v: "USDT" },
        { k: t("wallet.factDepositFee"), v: <span className="mono">{limits ? `${fmtUsdt(limits.deposit_fee)} USDT` : "—"}</span> },
        { k: t("wallet.factDepositMin"), v: <span className="mono">{limits ? `${fmtUsdt(limits.deposit_min)} USDT` : "—"}</span> },
        { k: t("wallet.factCredit"), v: t("wallet.factCreditVal") },
      ]} />

      {limits && limits.deposit_fee > 0 && (
        <div className="callout info">
          <IconShield />
          <span>{t("wallet.depositFeeNote", {
            fee: fmtUsdt(limits.deposit_fee), min: fmtUsdt(limits.deposit_min),
          })}</span>
        </div>
      )}
    </>
  );
}

/* ── TRON · USDT withdrawal ──────────────────────────────────────────────── */

function TronWithdraw({
  limits, balance, onDone, onClose,
}: {
  limits: WithdrawLimits | null;
  balance: number;
  onDone: () => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [to, setTo] = useState("");
  const [amount, setAmount] = useState("");
  const [err, setErr] = useState("");
  const [ok, setOk] = useState("");
  const [busy, setBusy] = useState(false);

  const micros = Math.round(parseFloat(amount || "0") * 1_000_000);
  const addrValid = TRON_RE.test(to.trim());
  const min = limits?.withdraw_min ?? 0;
  const fee = limits?.withdraw_fee ?? 0;
  const maxSingle = limits?.withdraw_max_single ?? 0;
  const belowMin = micros > 0 && min > 0 && micros < min;
  const overSingle = maxSingle > 0 && micros > maxSingle;
  const amountValid = micros > 0 && micros <= balance && !belowMin && !overSingle;

  async function withdraw() {
    setErr(""); setOk("");
    if (!addrValid) return setErr(t("wallet.invalidAddress"));
    if (belowMin) return setErr(t("wallet.belowMin", { amount: fmtUsdt(min) }));
    if (overSingle) return setErr(t("wallet.overSingle", { amount: fmtUsdt(maxSingle) }));
    if (!amountValid) return setErr(t("wallet.invalidAmount"));
    setBusy(true);
    try {
      const r = await invoke<{ withdrawal_id: number; status: string }>("wallet_withdraw", {
        chain: "tron", toAddress: to.trim(), amount: micros,
      });
      setOk(t("wallet.withdrawOk", { id: r.withdrawal_id }));
      setTo(""); setAmount("");
      onDone();
    } catch (e) {
      setErr(String((e as Error).message));
    } finally {
      setBusy(false);
    }
  }

  // Once it is queued there is nothing left to fill in; showing the empty form
  // under the receipt invites a second, accidental withdrawal.
  if (ok) {
    return (
      <div className="fade-in">
        <div className="callout info"><IconShield /><span>{ok}</span></div>
        <button className="btn block lg" onClick={onClose}>{t("wallet.dlgDone")}</button>
      </div>
    );
  }

  return (
    <>
      <FactGrid facts={[
        { k: t("wallet.factMin"), v: <span className="mono">{limits ? `${fmtUsdt(limits.withdraw_min)} USDT` : "—"}</span> },
        { k: t("wallet.factMaxSingle"), v: <span className="mono">{limits ? `${fmtUsdt(limits.withdraw_max_single)} USDT` : "—"}</span> },
        { k: t("wallet.factMaxDaily"), v: <span className="mono">{limits ? `${fmtUsdt(limits.withdraw_max_daily)} USDT` : "—"}</span> },
      ]} />

      <div className="field">
        <label>{t("wallet.withdrawAddress")}</label>
        <input className={`input mono ${to.trim() && !addrValid ? "invalid" : ""}`} value={to}
          onChange={(e) => setTo(e.target.value)} placeholder="T…" spellCheck={false} />
        <div className={`hint${to.trim() && !addrValid ? " bad" : ""}`}>
          {to.trim() && !addrValid ? t("wallet.invalidAddress") : t("wallet.addressHint")}
        </div>
      </div>

      <div className="field">
        <label>{t("wallet.withdrawAmount")}</label>
        <div className="input-row">
          <input className={`input mono ${amount && !amountValid ? "invalid" : ""}`} value={amount}
            onChange={(e) => setAmount(e.target.value)} placeholder="0.00" inputMode="decimal" />
          <button className="btn ghost" onClick={() => setAmount((balance / 1_000_000).toString())}
            disabled={balance <= 0}>{t("wallet.max")}</button>
        </div>
        <div className="hint">{t("wallet.availableHint", { amount: fmtUsdt(balance) })}</div>
      </div>

      {/* What actually happens on submit, spelled out before the button. */}
      <div className="wd-summary">
        <div className="wd-line">
          <span>{t("wallet.sumAmount")}</span>
          <span className="mono tabular">{micros > 0 ? fmtUsdt(micros) : "0.00"} USDT</span>
        </div>
        {fee > 0 && (
          <>
            <div className="wd-line">
              <span>{t("wallet.sumFee")}</span>
              <span className="mono tabular">−{fmtUsdt(fee)} USDT</span>
            </div>
            {/* The fee is deducted from the amount above, so state the arriving
                figure rather than letting the user find it on the explorer. */}
            <div className="wd-line strong">
              <span>{t("wallet.sumNet")}</span>
              <span className="mono tabular">{amountValid ? fmtUsdt(micros - fee) : "—"} USDT</span>
            </div>
          </>
        )}
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
        <div className="callout warn"><IconShield /><span>{t("wallet.whitelistOnly")}</span></div>
      )}

      <button className="btn block lg" onClick={withdraw}
        disabled={!inTauri || busy || !addrValid || !amountValid}>
        {busy ? <IconRefresh className="spin" /> : <IconArrowRight />}
        {busy ? t("wallet.withdrawing") : t("wallet.withdrawSubmit")}
      </button>
      <Err>{err}</Err>
    </>
  );
}
