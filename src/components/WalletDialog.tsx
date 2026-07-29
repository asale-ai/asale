// Money in and money out, as a dialog rather than a second card on the wallet
// page. The page now shows one thing — what you have — and the two verbs hang
// off it; the funding rails live behind them.
//
// One view, not a wizard: the rails are tabs and the selected rail is fully
// rendered straight away — the deposit address and its QR are fetched on open,
// not put behind a button. `METHODS` is the extension point; adding a rail
// means adding an entry there and nothing else — the deposit and withdraw
// bodies are written against the rail, not against a chain.
//
// Withdrawal runs at a fixed body height, so switching rails never resizes the
// dialog under the cursor. Deposit is laid out to fit instead: both its rails
// render the same blocks at the same height, so the body sizes to its content
// and only scrolls on a short window — a scrollbar over a QR reads as "there is
// more to do down there" when there is not.
//
// Kept in sync with asale-web's `components/WalletDialog.tsx` — same rails,
// same copy keys, same warning. If you change one, change the other.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri, fmtUsdt, isSolanaAddress,
  type DepositSession, type WalletHistory, type WithdrawLimits,
} from "../lib";
import { qrPayload, walletsFor, type WalletBrand } from "../lib/wallets";
import { PayQr } from "./PayQr";
import { CopyChip, Err, FactGrid, Skeleton } from "../ui";
import { errText } from "../errors";
import {
  IconX, IconArrowRight, IconRefresh, IconInfo, IconShield, IconWallet, IconCheck,
} from "../icons";

const TRON_RE = /^T[1-9A-HJ-NP-Za-km-z]{33}$/;

export type WalletMode = "deposit" | "withdraw";

/** One funding rail. `id` drives the copy keys; `chain` drives every call. */
interface Method {
  id: string;
  chain: string;
  modes: WalletMode[];
  /** Network line, and the only thing that must be read before sending. */
  network: string;
  placeholder: string;
  isAddress: (s: string) => boolean;
  icon: React.ReactNode;
}

const METHODS: Method[] = [
  {
    id: "sol_usdt",
    chain: "solana",
    modes: ["deposit", "withdraw"],
    network: "Solana · SPL",
    placeholder: "4Nd1…",
    isAddress: isSolanaAddress,
    icon: <IconWallet />,
  },
  {
    id: "tron_usdt",
    chain: "tron",
    modes: ["deposit", "withdraw"],
    network: "TRON · TRC20",
    placeholder: "T…",
    isAddress: (s) => TRON_RE.test(s.trim()),
    icon: <IconWallet />,
  },
];

export function WalletDialog({
  mode, limits, balance, onClose, onDone,
}: {
  mode: WalletMode | null;
  /** The full history payload: it carries every rail's rules plus the default. */
  limits: WalletHistory | null;
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
  limits: WalletHistory | null;
  balance: number;
  onClose: () => void;
  onDone: () => void;
}) {
  const { t } = useTranslation();
  // The server decides which rail to steer users toward, so it leads the tab
  // strip. Until the history lands, the shipped order stands in.
  const available = useMemo(() => {
    const rails = METHODS.filter((m) => m.modes.includes(mode));
    const first = limits?.default_chain;
    return first ? [...rails].sort((a, b) => Number(b.chain === first) - Number(a.chain === first)) : rails;
  }, [mode, limits?.default_chain]);
  const [tab, setTab] = useState("");
  const picked = available.find((m) => m.id === tab) ?? available[0] ?? null;
  const railLimits = limits?.chains?.find((c) => c.chain === picked?.chain) ?? null;
  const title = t(mode === "deposit" ? "wallet.tabDeposit" : "wallet.tabWithdraw");

  return (
    <div className="modal-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      {/* Deposit runs a wallet picker beside the QR, which needs a second
          column; withdrawal is a form and stays narrow. */}
      <div className={`modal wdlg${mode === "deposit" ? " wide" : ""}`} role="dialog" aria-modal="true" aria-label={title}>
        <div className="modal-head">
          <h3>{title}</h3>
          <button type="button" className="modal-x" onClick={onClose} title={t("wallet.dlgClose")}>
            <IconX />
          </button>
        </div>

        <div className="wdlg-tabs">
          <div className="segmented">
            {available.map((m) => (
              <button key={m.id} className={m.id === picked?.id ? "active" : ""} onClick={() => setTab(m.id)}>
                {m.icon}{t(`wallet.method_${m.id}`)}
              </button>
            ))}
          </div>
        </div>

        {/* Fixed height: the tab strip must not move when the body changes.
            Keyed on the rail so switching tabs starts from a clean form and
            refetches the address rather than showing the other chain's. */}
        <div className="wdlg-body">
          {picked && (mode === "deposit"
            ? <RailDeposit key={picked.id} method={picked} limits={railLimits} onDone={onDone} />
            : <RailWithdraw key={picked.id} method={picked} limits={railLimits} balance={balance}
                onDone={onDone} onClose={onClose} />)}
        </div>
      </div>
    </div>
  );
}

/* ── Deposit ─────────────────────────────────────────────────────────────── */

/** How often to ask the server whether the money landed. Each poll is one
 *  indexed read plus at most three tiny UPDATEs, so this is cheap; 4s is short
 *  enough that the arrival feels immediate. */
const POLL_MS = 4000;

/** Debounce on the amount field, so typing "10.50" opens one session, not five. */
const AMOUNT_DEBOUNCE_MS = 600;

/** The top-ups people actually make, in USDT. One tap beats typing, and the
 *  free field beside them still takes any figure — including none. */
const AMOUNT_PRESETS = [10, 20, 50, 100];

function RailDeposit({
  method, limits, onDone,
}: {
  method: Method;
  limits: WithdrawLimits | null;
  /** The balance moved — the page behind should refetch. */
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const wallets = useMemo(() => walletsFor(method.chain), [method.chain]);
  const [walletId, setWalletId] = useState("");
  const wallet = wallets.find((w) => w.id === walletId) ?? wallets[0] ?? null;

  /* The picker is one value in two places: a chip is either lit or the free
     field holds the figure. Kept apart so typing "10" by hand does not silently
     jump into the chip and empty the box under the cursor. */
  const [preset, setPreset] = useState<number | null>(null);
  const [custom, setCustom] = useState("");
  const amountInput = preset != null ? String(preset) : custom;
  const clearAmount = useCallback(() => { setPreset(null); setCustom(""); }, []);
  /* The amount the session was actually opened for, settled on the debounce —
     a half-typed "1" must not tear down the session meant for "10.50". */
  const [amount, setAmount] = useState<number | null>(null);

  const [session, setSession] = useState<DepositSession | null>(null);
  const [err, setErr] = useState("");
  const [reopen, setReopen] = useState(0);
  /* An escape hatch for a wallet whose scanner does not take the payment URI
     after all. Nobody should be stuck staring at a QR that will not scan. */
  const [forceAddress, setForceAddress] = useState(false);

  const micros = useMemo(() => {
    const n = parseFloat(amountInput);
    return amountInput.trim() && Number.isFinite(n) && n > 0 ? Math.round(n * 1_000_000) : null;
  }, [amountInput]);

  useEffect(() => {
    const id = setTimeout(() => setAmount(micros), AMOUNT_DEBOUNCE_MS);
    return () => clearTimeout(id);
  }, [micros]);

  const restart = useCallback(() => {
    setSession(null);
    setReopen((v) => v + 1);
  }, []);

  /* One session per (rail, amount): a stale session would still be asking for
     the previous figure while the sheet shows the new one. Only the newest
     request may land, which `seq` enforces — a slow first call must not
     overwrite the session a later one already delivered. */
  const seqRef = useRef(0);
  useEffect(() => {
    if (!inTauri) return;
    const seq = ++seqRef.current;
    invoke<DepositSession>("wallet_deposit_session", { chain: method.chain, amount })
      .then((s) => {
        if (seq !== seqRef.current) return;
        setSession(s);
        setErr("");
      })
      .catch((e) => {
        if (seq === seqRef.current) setErr(errText(e));
      });
  }, [method.chain, amount, reopen]);

  /* Poll until the money is in. `credited` is terminal; `expired` stops the
     watch but not the payment, so the copy says so rather than implying the
     address went dead. */
  const watching = session != null && (session.status === "pending" || session.status === "matched");
  // Ref rather than a dependency: `onDone` refetches the page, which would
  // otherwise re-arm this effect on every tick.
  const onDoneRef = useRef(onDone);
  useEffect(() => { onDoneRef.current = onDone; }, [onDone]);

  const sessionRef = session?.ref;
  useEffect(() => {
    if (!watching || !sessionRef) return;
    let stop = false;
    const id = setInterval(async () => {
      // A hidden window cannot show the result anyway, and the next poll after
      // it is foregrounded catches up in one call.
      if (document.visibilityState === "hidden") return;
      try {
        const next = await invoke<DepositSession>("wallet_deposit_session_get", { sessionRef });
        if (stop) return;
        setSession((prev) => {
          if (next.status === "credited" && prev?.status !== "credited") onDoneRef.current();
          return next;
        });
      } catch {
        // A failed poll is not worth surfacing: the next one is 4s away, and an
        // error banner over a valid QR would read as "do not pay".
      }
    }, POLL_MS);
    return () => { stop = true; clearInterval(id); };
  }, [watching, sessionRef]);

  // The QR follows whichever wallet is selected — that is the whole point of
  // the picker (see lib/wallets.ts).
  const payload = session ? qrPayload(forceAddress ? null : wallet, session.pay_uri, session.address) : "";
  const isPayUri = payload !== "" && payload === session?.pay_uri;

  const groups = useMemo(() => ([
    { kind: "wallet" as const, items: wallets.filter((w) => w.kind === "wallet") },
    { kind: "exchange" as const, items: wallets.filter((w) => w.kind === "exchange") },
  ]), [wallets]);

  return (
    <>
      {/* The network to send over, above everything else — it has to be read
          before the QR is scanned. Informational, not alarming: an alarm here
          reads as "this is risky" and stops people funding at all. */}
      <div className="depwarn">
        <div className="depwarn-head">
          <IconInfo />
          <span className="depwarn-title">{t("wallet.depositWarnTitle", { network: method.network })}</span>
        </div>
        <p className="depwarn-body">{t("wallet.depositWarnDesc", { network: method.network })}</p>
      </div>

      {/* Optional: an amount turns the QR into a filled-in payment request on
          wallets that support one. Empty means "send whatever", which is what
          an exchange withdrawal does anyway once its own fee is taken. */}
      <div className="field amt-field">
        <label>{t("wallet.payAmountLabel")}</label>
        <div className="amt-picker">
          {AMOUNT_PRESETS.map((v) => (
            <button key={v} type="button" className={`amt-chip${preset === v ? " active" : ""}`}
              /* Tapping the lit chip again is how you get back to "any amount"
                 without hunting for a clear button. */
              onClick={() => { setPreset(preset === v ? null : v); setCustom(""); }}>
              {v} <span className="amt-chip-unit">USDT</span>
            </button>
          ))}
          <div className="amt-custom">
            <input className="input mono" value={custom} inputMode="decimal"
              placeholder={t("wallet.payAmountCustom")}
              onChange={(e) => { setCustom(e.target.value); setPreset(null); }} />
            <span className="amt-unit">USDT</span>
          </div>
        </div>
        <div className="hint">
          {micros == null
            ? t("wallet.payAmountHintAny")
            : limits && micros < limits.deposit_min
              ? t("wallet.payAmountHintBelowMin", { amount: fmtUsdt(limits.deposit_min) })
              : t("wallet.payAmountHintSet")}
        </div>
      </div>

      {err ? (
        <Err>{err}</Err>
      ) : (
        <div className="paygrid">
          <div className="paygrid-wallets">
            <span className="paygrid-label">{t("wallet.payPickWallet")}</span>
            {groups.map((g) => g.items.length > 0 && (
              <div key={g.kind} className="wallet-group">
                <span className="wallet-group-head">
                  {t(g.kind === "wallet" ? "wallet.payGroupWallets" : "wallet.payGroupExchanges")}
                </span>
                {g.items.map((w) => (
                  <button key={w.id} className={`wallet-row${w.id === wallet?.id ? " active" : ""}`}
                    onClick={() => { setWalletId(w.id); setForceAddress(false); }}>
                    <WalletMark wallet={w} />
                    <span className="wallet-name">{w.name || t("wallet.payOtherWallet")}</span>
                  </button>
                ))}
              </div>
            ))}
          </div>

          <div className="paygrid-pay">
            {!session ? (
              <div className="dep-result">
                <Skeleton w={184} h={184} r={12} />
                <Skeleton h={34} r={8} />
              </div>
            ) : session.status === "credited" || session.status === "matched" ? (
              <PayReceipt session={session} onRestart={() => { clearAmount(); restart(); }} />
            ) : (
              <>
                <p className="pay-hint">
                  {isPayUri
                    ? t("wallet.payScanWith", { wallet: wallet?.name ?? "" })
                    : wallet?.kind === "exchange"
                      ? t("wallet.payScanExchange", { wallet: wallet.name })
                      : t("wallet.payScanAddress")}
                </p>
                <PayQr payload={payload} alt={t("wallet.qrAlt")} size={184} />

                {/* Exchanges do not read payment requests, so the useful thing
                    to show is the steps their app actually needs. */}
                {wallet?.kind === "exchange" && (
                  <ol className="pay-steps">
                    <li>{t("wallet.payStepExchange1", { wallet: wallet.name })}</li>
                    <li>{t("wallet.payStepExchange2", { network: method.network })}</li>
                    <li>{t("wallet.payStepExchange3")}</li>
                  </ol>
                )}

                <div className="pay-addr">
                  <span className="pay-addr-label">{t("wallet.payAddressLabel")}</span>
                  <CopyChip value={session.address} wrap />
                </div>

                {/* A payment URI that a scanner rejects is a dead end unless
                    there is a way back to the universal code. */}
                {session.pay_uri && wallet?.qr === "pay" && (
                  <button className="pay-fallback" onClick={() => setForceAddress((v) => !v)}>
                    {forceAddress ? t("wallet.payUsePayUri") : t("wallet.payUseAddressQr")}
                  </button>
                )}

                {session.status === "expired" ? (
                  <div className="pay-status">
                    <span>{t("wallet.payExpired")}</span>
                    <button className="btn ghost sm" onClick={restart}>
                      <IconRefresh />{t("wallet.payRestart")}
                    </button>
                  </div>
                ) : (
                  <div className="pay-status watching">
                    <span className="pay-pulse" aria-hidden="true" />
                    <span>{t("wallet.payWaiting")}</span>
                  </div>
                )}
              </>
            )}
          </div>
        </div>
      )}

      <FactGrid facts={[
        { k: t("wallet.factNetwork"), v: method.network },
        { k: t("wallet.factDepositFee"), v: <span className="mono">{limits ? `${fmtUsdt(limits.deposit_fee)} USDT` : "—"}</span> },
        { k: t("wallet.factDepositMin"), v: <span className="mono">{limits ? `${fmtUsdt(limits.deposit_min)} USDT` : "—"}</span> },
        { k: t("wallet.factCredit"), v: t("wallet.factCreditVal") },
      ]} />
    </>
  );
}

/** The coloured initial standing in for a wallet's logo (see lib/wallets.ts). */
function WalletMark({ wallet }: { wallet: WalletBrand }) {
  return (
    <span className="wallet-mark" style={{ background: wallet.color }} aria-hidden="true">
      {wallet.mark}
    </span>
  );
}

/** What the payer sees once a transfer is caught.
 *
 *  `matched` and `credited` are deliberately different screens: the money has
 *  arrived in both, but only the second is spendable, and someone watching
 *  this sheet wants to know which they are looking at. */
function PayReceipt({ session, onRestart }: { session: DepositSession; onRestart: () => void }) {
  const { t } = useTranslation();
  const credited = session.status === "credited";
  const received = session.deposit?.amount ?? null;
  return (
    <div className="pay-done fade-in">
      <span className={`pay-done-mark${credited ? " ok" : ""}`}>
        {credited ? <IconCheck /> : <IconRefresh className="spin" />}
      </span>
      <p className="pay-done-title">{t(credited ? "wallet.payCredited" : "wallet.payReceived")}</p>
      {received != null && <p className="pay-done-amount mono">{fmtUsdt(received)} USDT</p>}
      <p className="pay-done-desc">
        {credited
          ? t("wallet.payCreditedDesc")
          : t("wallet.payReceivedDesc", { n: session.deposit?.confirmations ?? 0 })}
      </p>
      {credited && <button className="btn ghost sm" onClick={onRestart}>{t("wallet.payAgain")}</button>}
    </div>
  );
}

/* ── Withdrawal ──────────────────────────────────────────────────────────── */

function RailWithdraw({
  method, limits, balance, onDone, onClose,
}: {
  method: Method;
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
  const addrValid = method.isAddress(to);
  const min = limits?.withdraw_min ?? 0;
  const fee = limits?.withdraw_fee ?? 0;
  const maxSingle = limits?.withdraw_max_single ?? 0;
  const belowMin = micros > 0 && min > 0 && micros < min;
  const overSingle = maxSingle > 0 && micros > maxSingle;
  const amountValid = micros > 0 && micros <= balance && !belowMin && !overSingle;

  async function withdraw() {
    setErr(""); setOk("");
    if (!addrValid) return setErr(t("wallet.invalidAddress", { network: method.network }));
    if (belowMin) return setErr(t("wallet.belowMin", { amount: fmtUsdt(min) }));
    if (overSingle) return setErr(t("wallet.overSingle", { amount: fmtUsdt(maxSingle) }));
    if (!amountValid) return setErr(t("wallet.invalidAmount"));
    setBusy(true);
    try {
      const r = await invoke<{ withdrawal_id: number; status: string }>("wallet_withdraw", {
        chain: method.chain, toAddress: to.trim(), amount: micros,
      });
      setOk(t("wallet.withdrawOk", { id: r.withdrawal_id }));
      setTo(""); setAmount("");
      onDone();
    } catch (e) {
      setErr(errText(e));
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
        { k: t("wallet.factNetwork"), v: method.network },
        { k: t("wallet.factMin"), v: <span className="mono">{limits ? `${fmtUsdt(limits.withdraw_min)} USDT` : "—"}</span> },
        { k: t("wallet.factMaxSingle"), v: <span className="mono">{limits ? `${fmtUsdt(limits.withdraw_max_single)} USDT` : "—"}</span> },
        { k: t("wallet.factMaxDaily"), v: <span className="mono">{limits ? `${fmtUsdt(limits.withdraw_max_daily)} USDT` : "—"}</span> },
      ]} />

      <div className="field">
        <label>{t("wallet.withdrawAddress")}</label>
        <input className={`input mono ${to.trim() && !addrValid ? "invalid" : ""}`} value={to}
          onChange={(e) => setTo(e.target.value)} placeholder={method.placeholder} spellCheck={false} />
        <div className={`hint${to.trim() && !addrValid ? " bad" : ""}`}>
          {to.trim() && !addrValid
            ? t("wallet.invalidAddress", { network: method.network })
            : t("wallet.addressHint")}
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
          <span>{method.network}</span>
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
