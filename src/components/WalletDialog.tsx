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
  invoke, inTauri, realTauri, fmtUsdt, isSolanaAddress,
  type CardLimits, type CardSession, type DepositSession, type WalletHistory, type WithdrawLimits,
} from "../lib";
import { qrPayload, walletsFor, type WalletBrand } from "../lib/wallets";
import { PayQr } from "./PayQr";
import { CopyChip, Err, FactGrid, Skeleton } from "../ui";
import { errText } from "../errors";
import {
  IconX, IconArrowRight, IconRefresh, IconInfo, IconShield, IconWallet, IconCheck, IconCard,
} from "../icons";

const TRON_RE = /^T[1-9A-HJ-NP-Za-km-z]{33}$/;

export type WalletMode = "deposit" | "withdraw";

/** One funding rail. `id` drives the copy keys; `chain` drives every call.
 *
 *  `card` is the rail that is not a chain: there is no address to send to, no
 *  network to warn about and no way out, so it carries `kind: "card"` and the
 *  body branches on that rather than on the chain name. Everything below the
 *  tab strip is written against the rail, which is what lets one deposit-only
 *  rail sit beside two symmetric ones. */
interface Method {
  id: string;
  chain: string;
  kind: "chain" | "card";
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
    kind: "chain",
    modes: ["deposit", "withdraw"],
    network: "Solana · SPL",
    placeholder: "4Nd1…",
    isAddress: isSolanaAddress,
    icon: <IconWallet />,
  },
  {
    id: "tron_usdt",
    chain: "tron",
    kind: "chain",
    modes: ["deposit", "withdraw"],
    network: "TRON · TRC20",
    placeholder: "T…",
    isAddress: (s) => TRON_RE.test(s.trim()),
    icon: <IconWallet />,
  },
  {
    id: "card",
    chain: "card",
    kind: "card",
    // Deposit only. The processor takes cards; it does not pay them, so there
    // is nothing to put behind a withdraw tab.
    modes: ["deposit"],
    network: "Visa · Mastercard",
    placeholder: "",
    isAddress: () => false,
    icon: <IconCard />,
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
    // The card rail only exists where a processor is configured; a deployment
    // without one must not show a tab that 503s. Until the history lands we do
    // not know, and a tab that appears a moment later is better than one that
    // appears and then cannot be paid with.
    const rails = METHODS.filter((m) => m.modes.includes(mode) && (m.kind !== "card" || !!limits?.card));
    const first = limits?.default_chain;
    // Chains sort by the server's preference; the card stays where it is —
    // last, because it is the one that costs extra.
    return first
      ? [...rails].sort((a, b) => Number(b.chain === first) - Number(a.chain === first))
      : rails;
  }, [mode, limits?.default_chain, limits?.card]);
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
            ? (picked.kind === "card"
                ? <CardDeposit key={picked.id} limits={limits?.card ?? null} onDone={onDone} />
                : <RailDeposit key={picked.id} method={picked} limits={railLimits} onDone={onDone} />)
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

/** Lit on open. Most top-ups land here anyway, and it means the QR is a
 *  filled-in payment request from the first frame rather than a bare address
 *  waiting for a decision. Tapping the lit chip still gets you back to "any
 *  amount". */
const DEFAULT_AMOUNT = 20;

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
  const [preset, setPreset] = useState<number | null>(DEFAULT_AMOUNT);
  const [custom, setCustom] = useState("");
  const amountInput = preset != null ? String(preset) : custom;
  const clearAmount = useCallback(() => { setPreset(null); setCustom(""); }, []);
  /* The amount the session was actually opened for, settled on the debounce —
     a half-typed "1" must not tear down the session meant for "10.50". Seeded
     with the lit chip so the first session is opened for it directly, rather
     than one for "any amount" that the debounce immediately replaces. */
  const [amount, setAmount] = useState<number | null>(DEFAULT_AMOUNT * 1_000_000);

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
              <PayReceipt
                session={session}
                fee={limits?.deposit_fee ?? null}
                onRestart={() => { clearAmount(); restart(); }}
              />
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
        // "0.00 USDT" is a true answer that reads as an oversight. A rail that
        // charges nothing should say so — it is the reason to pick it.
        { k: t("wallet.factDepositFee"), v: <span className="mono">{limits ? (limits.deposit_fee > 0 ? `${fmtUsdt(limits.deposit_fee)} USDT` : t("wallet.feeFree")) : "—"}</span> },
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
 *  this sheet wants to know which they are looking at.
 *
 *  The big figure is what reaches the balance, not what landed on chain. Those
 *  differ by the deposit fee, and showing the gross here was the whole reason a
 *  3 USDT top-up looked like it lost a dollar on the way in: the sheet said
 *  2.70 and the balance moved 1.70. The chain amount is still shown, demoted to
 *  the line that explains the difference. */
function PayReceipt({
  session, fee, onRestart,
}: {
  session: DepositSession;
  /** Flat deposit fee in micro-USDT; null until the limits land. */
  fee: number | null;
  onRestart: () => void;
}) {
  const { t } = useTranslation();
  const credited = session.status === "credited";
  const received = session.deposit?.amount ?? null;
  // Clamped exactly as the server clamps it (bin/chain.rs), so a deposit
  // smaller than the fee reads as the zero it will actually credit rather than
  // as a negative number no ledger will ever show.
  const charged = received != null && fee != null ? Math.min(Math.max(fee, 0), received) : null;
  const net = received != null && charged != null ? received - charged : received;
  return (
    <div className="pay-done fade-in">
      <span className={`pay-done-mark${credited ? " ok" : ""}`}>
        {credited ? <IconCheck /> : <IconRefresh className="spin" />}
      </span>
      <p className="pay-done-title">{t(credited ? "wallet.payCredited" : "wallet.payReceived")}</p>
      {net != null && <p className="pay-done-amount mono">{fmtUsdt(net)} USDT</p>}
      {/* Only worth a line when a fee was actually taken — on a rail that
          charges nothing, the breakdown would just repeat the figure above. */}
      {received != null && charged != null && charged > 0 && (
        <p className="pay-done-split">
          {t("wallet.payReceiptSplit", { gross: fmtUsdt(received), fee: fmtUsdt(charged) })}
        </p>
      )}
      <p className="pay-done-desc">
        {credited
          ? t("wallet.payCreditedDesc")
          : t("wallet.payReceivedDesc", { n: session.deposit?.confirmations ?? 0 })}
      </p>
      {credited && <button className="btn ghost sm" onClick={onRestart}>{t("wallet.payAgain")}</button>}
    </div>
  );
}

/* ── Card ────────────────────────────────────────────────────────────────── */

/**
 * Top up with a card, through the processor's own hosted checkout.
 *
 * The shape is deliberately different from a chain rail. There is no address,
 * no QR and nothing to watch for on a network: the user picks a figure, presses
 * one button, pays on the processor's page, and this sheet waits for the
 * webhook to land the credit.
 *
 * The one thing this screen exists to make honest is the *price*. The wallet is
 * credited the round figure on the chip, and the card is charged more than that
 * — so the surcharge is on its own line, in money, before the button rather
 * than after it. Sales tax is not in that figure and cannot be: the processor
 * is the merchant of record and works it out from the country the user enters
 * on its page, which happens after this one. Saying where it is decided is the
 * only honest thing to do; quoting a total we cannot compute would be worse
 * than the gap it was meant to close.
 */
function CardDeposit({
  limits, onDone,
}: {
  limits: CardLimits | null;
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const [preset, setPreset] = useState<number | null>(DEFAULT_AMOUNT);
  const [custom, setCustom] = useState("");
  const amountInput = preset != null ? String(preset) : custom;
  const [session, setSession] = useState<CardSession | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  const micros = useMemo(() => {
    const n = parseFloat(amountInput);
    return amountInput.trim() && Number.isFinite(n) && n > 0 ? Math.round(n * 1_000_000) : null;
  }, [amountInput]);

  /* The quote, computed the same way the server does. It is shown before the
     call is made — that is the point — and the reply carries the server's own
     figures, which then take over so what is confirmed is what was charged. */
  const quoted = useMemo(() => {
    if (micros == null || !limits) return null;
    const fee = Math.round((micros * limits.fee_bps) / 10_000) + limits.fee_flat;
    // Whole cents, rounded up, as the processor bills.
    const charge = Math.ceil((micros + fee) / 10_000) * 10_000;
    return { fee, charge };
  }, [micros, limits]);

  const fee = session?.fee ?? quoted?.fee ?? null;
  const charge = session?.charge ?? quoted?.charge ?? null;

  const tooSmall = micros != null && limits != null && micros < limits.deposit_min;
  const tooLarge = micros != null && limits != null && micros > limits.deposit_max;
  const payable = micros != null && !tooSmall && !tooLarge && !busy;

  async function pay() {
    if (!payable) return;
    setBusy(true); setErr("");
    try {
      const s = await invoke<CardSession>("wallet_card_session", { amount: micros, openLocal: realTauri });
      setSession(s);
      // In the browser the daemon cannot reach a window; open it here. In the
      // desktop build the daemon already opened the system browser, because a
      // third party's card form has no business inside our webview.
      if (!realTauri) window.open(s.checkout_url, "_blank", "noopener");
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(false);
    }
  }

  /* Same poll as a chain deposit — the session is the same object, and the
     credit still arrives from somewhere this page cannot see. */
  const watching = session != null && (session.status === "pending" || session.status === "matched");
  const onDoneRef = useRef(onDone);
  useEffect(() => { onDoneRef.current = onDone; }, [onDone]);
  const sessionRef = session?.ref;
  useEffect(() => {
    if (!watching || !sessionRef) return;
    let stop = false;
    const id = setInterval(async () => {
      if (document.visibilityState === "hidden") return;
      try {
        const next = await invoke<DepositSession>("wallet_deposit_session_get", { sessionRef });
        if (stop) return;
        setSession((prev) => {
          if (!prev) return prev;
          if (next.status === "credited" && prev.status !== "credited") onDoneRef.current();
          return { ...prev, ...next };
        });
      } catch {
        // Same reasoning as the chain rail: the next poll is 4s away.
      }
    }, POLL_MS);
    return () => { stop = true; clearInterval(id); };
  }, [watching, sessionRef]);

  if (session?.status === "credited") {
    return (
      // `fee` on the receipt is the chain rails' *carve-out* — money taken out
      // of what was credited. A card surcharge is charged on top instead, so
      // nothing was deducted from the figure that landed and the receipt has no
      // deduction line to draw.
      <PayReceipt session={session} fee={null} onRestart={() => { setSession(null); setErr(""); }} />
    );
  }

  return (
    <div className="card-pay">
      <div className="depwarn">
        <div className="depwarn-head">
          <IconInfo />
          <span className="depwarn-title">{t("wallet.cardTitle")}</span>
        </div>
        <p className="depwarn-body">{t("wallet.cardDesc")}</p>
      </div>

      {/* Same picker as the chain rails — one amount control in the product,
          not two that drift apart. */}
      <div className="field amt-field">
        <label>{t("wallet.payAmountLabel")}</label>
        <div className="amt-picker">
          {AMOUNT_PRESETS.map((v) => (
            <button key={v} type="button" className={`amt-chip${preset === v ? " active" : ""}`}
              onClick={() => { setPreset(v); setCustom(""); }}>
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
      </div>

      {/* The gap between the figure on the chip and the figure on the card.
          Named, itemised, and above the button — not a footnote under it. */}
      <div className="card-quote">
        <div className="cq-row">
          <span>{t("wallet.cardCredited")}</span>
          <span className="mono tabular">{micros != null ? fmtUsdt(micros) : "—"}</span>
        </div>
        <div className="cq-row">
          <span>{t("wallet.cardFee")}</span>
          <span className="mono tabular">{fee != null ? `+ ${fmtUsdt(fee)}` : "—"}</span>
        </div>
        <div className="cq-row total">
          <span>{t("wallet.cardCharge")}</span>
          <span className="mono tabular">{charge != null ? fmtUsdt(charge) : "—"}</span>
        </div>
        <p className="cq-note">{t("wallet.cardTaxNote")}</p>
      </div>

      {tooSmall && limits && (
        <Err>{t("wallet.cardMin", { min: fmtUsdt(limits.deposit_min) })}</Err>
      )}
      {tooLarge && limits && (
        <Err>{t("wallet.cardMax", { max: fmtUsdt(limits.deposit_max) })}</Err>
      )}
      <Err>{err}</Err>

      <button className="btn card-pay-btn" onClick={pay} disabled={!payable || !inTauri}>
        {busy ? t("wallet.cardOpening") : t("wallet.cardPay", { total: charge != null ? fmtUsdt(charge) : "" })}
        <IconArrowRight />
      </button>

      {session && (
        <div className="card-waiting">
          <span className="dot" />
          <span>{t("wallet.cardWaiting")}</span>
          {/* The popup can be blocked, or closed by accident. The link is the
              same checkout and stays usable until the session expires. */}
          <a href={session.checkout_url} target="_blank" rel="noopener noreferrer">
            {t("wallet.cardReopen")}
          </a>
        </div>
      )}
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
