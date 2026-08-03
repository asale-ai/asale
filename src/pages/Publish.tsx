import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri, realTauri, runOAuthFlow, submitOAuthCode, fmtTokens,
  type AccountStatus, type ClientStatus, type ImportAllResult, type Lane,
} from "../lib";
import { Card, Ok, Err, SkeletonRows, PageHead, IconAction, Empty, Mark, CopyChip } from "../ui";
import { IconTrash, IconShield, IconChip, IconRefresh, IconPlus, IconPencil, IconInfo } from "../icons";
import { errText } from "../errors";

/** Subscriptions connected by signing in through a loopback OAuth callback. */
const PROVIDERS = [
  { id: "claude", label: "Claude Code" },
  { id: "claude_work", label: "Claude Work" },
  { id: "codex", label: "Codex / OpenAI" },
  { id: "gemini", label: "Gemini" },
];

/** Subscriptions authorised by device code. Same two-step flow, except the
 *  user confirms a short code instead of being redirected back — which is why
 *  these two also work when the UI runs in a browser on another machine. */
const DEVICE_PROVIDERS = [
  { id: "kimi", label: "Kimi Code" },
  { id: "xai", label: "Grok CLI" },
];

/** The metered platform APIs, which issue keys rather than subscriptions.
 *  `keyUrl` is where the key is issued. */
const KEY_PROVIDERS = [
  { id: "kimi_api", label: "Moonshot API", keyUrl: "https://platform.moonshot.cn/console/api-keys" },
  { id: "xai_api", label: "xAI API", keyUrl: "https://console.x.ai" },
];

/** "in 4m 12s" — a countdown is what makes an auto-recovering pause read as
 *  "wait" rather than "broken". Returns "" once the instant has passed. */
const countdown = (until: number, now: number) => {
  const left = until - now;
  if (left <= 0) return "";
  const m = Math.floor(left / 60);
  const s = left % 60;
  return m > 0 ? `${m}m ${s}s` : `${s}s`;
};

/** Stable per-account key — a provider can hold several accounts. */
const keyOf = (a: { provider: string; account_id: string }) => `${a.provider}:${a.account_id}`;

/** Readable label for a credential store: `keychain:Claude Code-credentials` →
 *  `Claude Code-credentials`, a path → its last two segments. */
const shortSource = (s: string) =>
  s === "oauth" ? "Asale OAuth"
    : s.startsWith("keychain:") ? s.slice("keychain:".length)
    : s.split("/").filter(Boolean).slice(-2).join("/");

/** The legal range for a price ratio: the server clamps the market ratio to
 *  [0.10, 1.00], so a floor of 10 percent *of* list price is below every price a
 *  model can have and therefore means "sell at whatever the market pays".
 *
 *  A seller's decision is only ever "not below X" — there is no such thing as a
 *  price too good to accept — so the setting is that one number and nothing
 *  else. */
const RATIO_MIN = 10;
const RATIO_MAX = 100;
const noFloor = (lo: number) => lo <= RATIO_MIN;
const clampRatio = (n: number) => Math.min(RATIO_MAX, Math.max(RATIO_MIN, n));
/** The floor in force for an account, with "unset" reading as "any price". */
const floorOf = (a: { sell_min_ratio?: number | null }) => clampRatio(a.sell_min_ratio ?? RATIO_MIN);

/** Floors worth one click. */
const BAND_PRESETS = [RATIO_MIN, 50, 60, 70, 80];

/** Why a lane is or is not on the market, collapsed to the four cases the
 *  ranking chart draws differently.
 *
 *  These are *states*, not series, so they wear the app's status colours rather
 *  than a categorical palette — and each one carries its own words in the row
 *  beside the bar, because a colour on its own is not an answer. */
type LaneTone = "selling" | "price" | "blocked" | "off";

const laneTone = (l: Lane): LaneTone =>
  l.status === "selling" ? "selling"
    : l.status === "withheld" ? "price"
    : l.status === "off" ? "off"
    : "blocked";

/** How many models a chart shows before it needs asking. Enough that the whole
 *  price question is answerable at a glance for every provider we sell, without
 *  a hundred-row catalog burying the account below it. */
const RANK_VISIBLE = 8;

/** One subscription's models, ranked by what the market currently pays for
 *  them, and split into what is selling and what is not.
 *
 *  The bar is the price as a fraction of the vendor's list price, so a longer
 *  bar is more money, and the account's floor is drawn on the same scale as the
 *  zone those bars have to land in. That is the whole question this chart
 *  exists to answer: which of my models has the market pushed below the price I
 *  said I would sell at.
 *
 *  The scale is fixed at 0–100% rather than fitted to the data. Bars need a zero
 *  baseline to be read as lengths at all, and the floor marker is only
 *  meaningful against an axis that does not move when the prices do.
 */
function DiscountRank({
  lanes, floor, now, onResume, resuming,
}: {
  lanes: Lane[];
  floor: number;
  now: number;
  onResume: (lane: Lane) => void;
  resuming: Record<string, boolean>;
}) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const open = noFloor(floor);

  // Cheapest first: the models the market has pushed furthest down are the ones
  // a floor is about, so they are the ones worth reading first. A lane with no
  // known price sorts last — it has nothing to rank on.
  const rows = [...lanes].sort((a, b) => {
    const ra = a.ratio ?? 1e9;
    const rb = b.ratio ?? 1e9;
    return ra - rb || a.model.localeCompare(b.model);
  });
  const shown = expanded ? rows : rows.slice(0, RANK_VISIBLE);
  const hidden = rows.length - shown.length;
  const sellingCount = rows.filter((l) => l.status === "selling").length;
  const priceCount = rows.filter((l) => l.status === "withheld").length;
  const priced = rows.filter((l) => l.ratio != null).length;

  const stateText = (l: Lane): string => {
    if (l.status === "selling") return t("publish.rank.onSale");
    if (l.status === "withheld") return t("publish.rank.heldOnPrice");
    if (l.status === "off") return t("publish.rank.switchedOff");
    if (l.paused_reason) return t(`publish.lanePause.${l.paused_reason}`, { defaultValue: l.paused_reason });
    return t(`publish.laneStatus.${l.status}`, { defaultValue: l.status });
  };

  return (
    <div className="disc-rank">
      <div className="dr-head">
        <span className="dr-title">{t("publish.rank.title")}</span>
        <span className="dr-count">{t("publish.rank.onSaleOfTotal", { n: sellingCount, total: rows.length })}</span>
      </div>

      {/* No price for anything is a state of its own, and a very different one
          from "the market pays nothing" — which is what a chart of empty bars
          would otherwise be saying. */}
      {priced === 0 && <div className="dr-nodata">{t("publish.rank.noPrices")}</div>}

      <div className="dr-rows">
        {shown.map((l) => {
          const lk = `${l.provider}:${l.account_id}:${l.model}`;
          const tone = laneTone(l);
          const r = l.ratio;
          const back = countdown(Math.max(l.resume_at, l.cooldown_until ?? 0), now);
          const state = stateText(l);
          return (
            <div
              key={lk}
              className={`dr-row ${tone}${l.requires_user ? " attention" : ""}`}
              title={`${l.model} · ${state}${l.last_error ? `\n${l.last_error}` : ""}`}
            >
              <span className="dr-model mono">{l.model}</span>
              <div className="dr-track">
                {/* The zone the operator said they would sell in: everything at
                    or above the floor, with no upper edge to draw — there is no
                    such thing as a price too good to accept. Left out when no
                    floor is set: a tint over the whole track would read as a
                    threshold where there is none. */}
                {!open && (
                  <span
                    className="dr-band to-top"
                    style={{ left: `${floor}%`, width: `${RATIO_MAX - floor}%` }}
                  />
                )}
                {r != null && (
                  <span className="dr-fill" style={{ width: `${clampRatio(r)}%` }} />
                )}
              </div>
              <span className="dr-val mono tabular">{r == null ? "—" : `${r}%`}</span>
              <span className="dr-state">
                {state}
                {back && <span className="dr-back"> · {back}</span>}
              </span>
              {l.requires_user && (
                <button
                  className="lane-resume"
                  onClick={() => onResume(l)}
                  disabled={!inTauri || !!resuming[lk]}
                  title={t("publish.laneResumeHint")}
                >
                  {t("publish.laneResume")}
                </button>
              )}
            </div>
          );
        })}
      </div>

      {/* A cap that hides rows has to say so — a chart that quietly stops at
          eight reads as a complete answer when it is not. */}
      {hidden > 0 && (
        <button className="lane-resume dr-more" onClick={() => setExpanded(true)}>
          {t("publish.rank.showAll", { n: hidden })}
        </button>
      )}
      {expanded && rows.length > RANK_VISIBLE && (
        <button className="lane-resume dr-more" onClick={() => setExpanded(false)}>
          {t("publish.rank.showLess")}
        </button>
      )}

      <div className="dr-foot">
        <span className="dr-key">
          <i className="dr-swatch selling" /> {t("publish.rank.onSale")}
        </span>
        <span className="dr-key">
          <i className="dr-swatch price" /> {t("publish.rank.heldOnPrice")}
        </span>
        <span className="dr-key">
          <i className="dr-swatch blocked" /> {t("publish.rank.otherwiseOff")}
        </span>
        <span className="dr-scale">
          {open ? t("publish.rank.noBand") : t("publish.rank.bandFloor", { lo: floor })}
          {priceCount > 0 && ` · ${t("publish.rank.heldCount", { n: priceCount })}`}
        </span>
      </div>
    </div>
  );
}

/** Selling is per subscription account — there is no device-wide sell switch.
 *  The market session simply follows these switches (the daemon connects on the
 *  first account switched on and disconnects with the last), and the link's own
 *  state is shown by the global status widget in the top bar. */
export function Publish() {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState("");
  const [err, setErr] = useState("");

  const [accounts, setAccounts] = useState<AccountStatus[]>([]);
  const [acctLoading, setAcctLoading] = useState(inTauri);
  const [acctErr, setAcctErr] = useState("");
  /** Accounts with a sell action in flight, keyed by `provider:account_id`. */
  const [pending, setPending] = useState<Record<string, boolean>>({});

  // Per-model lane state. `now` ticks once a second so the countdowns on
  // cooling lanes actually count down between the 4s account polls.
  const [lanes, setLanes] = useState<Lane[]>([]);
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  const [resuming, setResuming] = useState<Record<string, boolean>>({});

  // Per-account daily cap drafts (edited independently of the switch). The cap
  // reads as a value next to the other account facts and only turns into an
  // input once the pencil is clicked — a permanently open field reads as
  // "unsaved" and does not line up with the read-only facts beside it.
  const [limitDraft, setLimitDraft] = useState<Record<string, string>>({});
  const [limitSaved, setLimitSaved] = useState("");
  const [limitEditing, setLimitEditing] = useState("");

  // Per-account price-floor drafts, edited on the same pencil pattern as the
  // cap above. One number: the share of list price at or above which this
  // subscription sells.
  const [bandDraft, setBandDraft] = useState<Record<string, string>>({});
  const [bandSaved, setBandSaved] = useState("");
  const [bandEditing, setBandEditing] = useState("");

  // The code a device-code login is waiting on, shown until it completes.
  const [deviceCode, setDeviceCode] = useState<{ provider: string; code: string; url: string } | null>(null);
  /** The in-flight loopback login, so its code can be pasted back when the
   *  callback lands on a machine that is not the daemon's. */
  const [pasteFlow, setPasteFlow] = useState<{ provider: string; flowId: string } | null>(null);
  const [pasteDraft, setPasteDraft] = useState("");

  // API-key connect: which vendor's form is open, and its draft.
  const [keyProvider, setKeyProvider] = useState("");
  const [keyDraft, setKeyDraft] = useState("");
  const [keyLabel, setKeyLabel] = useState("");

  // Local-CLI import: the daemon runs it at startup, this only re-runs it.
  const [rescanning, setRescanning] = useState(false);
  const [importMsg, setImportMsg] = useState("");
  const [importWarnings, setImportWarnings] = useState<string[]>([]);
  // Tools currently buying. Their local accounts are left out of the list
  // below, and an account that vanished needs an explanation, not silence.
  // Polled with the accounts would mean a keychain + filesystem scan every few
  // seconds, so this is read once and refreshed when the user rescans.
  const [buyingTools, setBuyingTools] = useState<string[]>([]);
  const [importErr, setImportErr] = useState("");

  const loadAccounts = useCallback(() => {
    if (!inTauri) return;
    invoke<AccountStatus[]>("list_accounts")
      .then((list) => {
        setAccounts(list);
        setAcctErr("");
        // Seed each account's cap input once; don't clobber an in-progress edit.
        setLimitDraft((d) => {
          const next = { ...d };
          for (const a of list) if (next[keyOf(a)] === undefined) next[keyOf(a)] = String(a.sell_daily_limit || 0);
          return next;
        });
        setBandDraft((d) => {
          const next = { ...d };
          for (const a of list) if (next[keyOf(a)] === undefined) next[keyOf(a)] = String(floorOf(a));
          return next;
        });
      })
      .catch((e) => setAcctErr(errText(e)))
      .finally(() => setAcctLoading(false));
    invoke<{ lanes: Lane[] }>("list_lanes")
      .then((r) => setLanes(r.lanes))
      .catch(() => {});
  }, []);

  const loadBuying = useCallback(() => {
    if (!inTauri) return;
    invoke<ClientStatus>("client_status")
      .then((s) => setBuyingTools(s.buying))
      .catch(() => {});
  }, []);

  useEffect(loadBuying, [loadBuying]);

  useEffect(() => {
    if (!inTauri) return;
    loadAccounts();
    const id = setInterval(loadAccounts, 4000);
    const tick = setInterval(() => setNow(Math.floor(Date.now() / 1000)), 1000);
    return () => { clearInterval(id); clearInterval(tick); };
  }, [loadAccounts]);

  /** Put a lane the operator has fixed back on the market. */
  async function resume(lane: Lane) {
    const k = `${lane.provider}:${lane.account_id}:${lane.model}`;
    setResuming((r) => ({ ...r, [k]: true }));
    setAcctErr("");
    try {
      await invoke("resume_lane", {
        provider: lane.provider,
        accountId: lane.account_id,
        model: lane.model,
      });
      loadAccounts();
    } catch (e) {
      setAcctErr(errText(e));
    } finally {
      setResuming((r) => ({ ...r, [k]: false }));
    }
  }

  /** Flip one account's sell switch, or save one of its selling terms, via the
   *  same RPC. Terms left out are kept as they are by the daemon. */
  async function setSell(
    a: AccountStatus,
    enabled: boolean,
    terms: { dailyLimit?: number; minRatio?: number; maxRatio?: number } = {},
  ) {
    const k = keyOf(a);
    const { dailyLimit, minRatio, maxRatio } = terms;
    setAcctErr("");
    setPending((p) => ({ ...p, [k]: true }));
    // Optimistic: the list refreshes on a 4s poll, too slow for a toggle.
    setAccounts((list) =>
      list.map((x) => (keyOf(x) === k
        ? {
            ...x,
            sell_enabled: enabled,
            sell_daily_limit: dailyLimit ?? x.sell_daily_limit,
            sell_min_ratio: minRatio ?? x.sell_min_ratio,
            sell_max_ratio: maxRatio ?? x.sell_max_ratio,
          }
        : x)),
    );
    try {
      await invoke("set_account_sell", {
        provider: a.provider,
        accountId: a.account_id,
        enabled,
        ...(dailyLimit === undefined ? {} : { dailyLimit }),
        ...(minRatio === undefined ? {} : { minRatio }),
        ...(maxRatio === undefined ? {} : { maxRatio }),
      });
      loadAccounts();
    } catch (e) {
      setAcctErr(errText(e));
      loadAccounts(); // roll the optimistic update back to server truth
    } finally {
      setPending((p) => ({ ...p, [k]: false }));
    }
  }

  async function saveLimit(a: AccountStatus) {
    const raw = parseInt(limitDraft[keyOf(a)] ?? "0", 10);
    const dailyLimit = Number.isFinite(raw) && raw > 0 ? raw : 0;
    setLimitEditing("");
    await setSell(a, a.sell_enabled, { dailyLimit });
    setLimitSaved(keyOf(a));
    setTimeout(() => setLimitSaved(""), 2000);
  }

  /** Save one account's price floor. An empty or unreadable number falls back to
   *  the bottom of the range, so a half-typed form means "any price" rather than
   *  a floor nobody chose — the failure that costs nothing, not the one that
   *  quietly takes the subscription off the market.
   *
   *  The ceiling always goes up with it: the daemon still holds a band, and
   *  anything less than list price there would withhold models on a rule this
   *  page no longer offers a way to see or unset. */
  async function saveBand(a: AccountStatus) {
    const raw = parseInt(bandDraft[keyOf(a)] ?? "", 10);
    const minRatio = Number.isFinite(raw) ? clampRatio(raw) : RATIO_MIN;
    setBandEditing("");
    setBandDraft((d) => ({ ...d, [keyOf(a)]: String(minRatio) }));
    await setSell(a, a.sell_enabled, { minRatio, maxRatio: RATIO_MAX });
    setBandSaved(keyOf(a));
    setTimeout(() => setBandSaved(""), 2000);
  }

  /** Open the floor editor on the value currently in force, for the same reason
   *  `editLimit` does. */
  function editBand(a: AccountStatus) {
    const k = keyOf(a);
    setBandDraft((d) => ({ ...d, [k]: String(floorOf(a)) }));
    setBandEditing(k);
  }

  /** Open the cap editor on the value currently in force (so cancelling an edit
   *  and reopening never resumes a half-typed number). */
  function editLimit(a: AccountStatus) {
    const k = keyOf(a);
    setLimitDraft((d) => ({ ...d, [k]: String(a.sell_daily_limit || 0) }));
    setLimitEditing(k);
  }

  async function connect(provider: string) {
    setErr(""); setMsg(""); setBusy(true); setPasteDraft("");
    try {
      // Two-step: the daemon opens/returns the authorize URL, we poll for the
      // loopback callback + token exchange to finish.
      const r = await runOAuthFlow<{ account_id: string }>(
        "oauth_login",
        { provider },
        (start) => setPasteFlow({ provider, flowId: start.flow_id }),
      );
      setMsg(t("publish.connected", { provider, account: r.account_id }));
      loadAccounts();
    } catch (e) { setErr(errText(e)); } finally { setBusy(false); setPasteFlow(null); }
  }

  /** Finish a login whose callback never arrived, from what the user copied out
   *  of the browser's address bar. The daemon takes it from there — the poll in
   *  `connect` above is still running and picks up the result. */
  async function submitPastedCode() {
    if (!pasteFlow || !pasteDraft.trim()) return;
    setErr("");
    try {
      await submitOAuthCode(pasteFlow.flowId, pasteDraft.trim());
      setPasteDraft("");
    } catch (e) { setErr(errText(e)); }
  }

  /** Device-code login. Identical two-step flow, except the user confirms a
   *  short code rather than being redirected back — so the code has to stay on
   *  screen for the whole wait, not just be opened and forgotten. */
  async function connectDevice(provider: string) {
    setErr(""); setMsg(""); setBusy(true); setDeviceCode(null);
    try {
      const r = await runOAuthFlow<{ account_id: string }>(
        "oauth_device_login",
        { provider },
        (start) => setDeviceCode({ provider, code: start.user_code ?? "", url: start.auth_url }),
      );
      setMsg(t("publish.connected", { provider, account: r.account_id }));
      loadAccounts();
    } catch (e) { setErr(errText(e)); } finally { setBusy(false); setDeviceCode(null); }
  }

  /** Save a pasted API key as an account. The daemon checks it against the
   *  vendor first, so a dead key is refused here rather than failing every task
   *  it later gets matched to. */
  async function connectKey() {
    setErr(""); setMsg(""); setBusy(true);
    try {
      const r = await invoke<{ account_id: string }>("connect_api_key", {
        provider: keyProvider,
        apiKey: keyDraft,
        ...(keyLabel.trim() ? { label: keyLabel.trim() } : {}),
      });
      setMsg(t("publish.connected", { provider: keyProvider, account: r.account_id }));
      setKeyProvider(""); setKeyDraft(""); setKeyLabel("");
      loadAccounts();
    } catch (e) { setErr(errText(e)); } finally { setBusy(false); }
  }

  // Re-run the local-CLI import (same routine the daemon runs on startup) —
  // used when the user has just logged into a CLI and wants it picked up now.
  async function rescanCli() {
    setRescanning(true); setImportErr(""); setImportMsg(""); setImportWarnings([]);
    try {
      const r = await invoke<ImportAllResult>("import_cli_all");
      setImportMsg(
        r.imported.length > 0
          ? t("publish.cliImportedN", { n: r.imported.length })
          : t("publish.cliNone"),
      );
      setImportWarnings(r.warnings);
      loadBuying();
      if (r.errors.length > 0) setImportErr(r.errors.map((e) => `${e.provider}: ${e.error}`).join("; "));
      loadAccounts();
    } catch (e) { setImportErr(errText(e)); } finally { setRescanning(false); }
  }

  async function removeAccount(a: AccountStatus) {
    setAcctErr("");
    try { await invoke<boolean>("remove_account", { provider: a.provider, accountId: a.account_id }); loadAccounts(); }
    catch (e) { setAcctErr(errText(e)); }
  }

  const statusPill = (s: AccountStatus["status"]) => {
    const cls = s === "available" ? "on" : s === "cooldown" || s === "exhausted" ? "warn" : "off";
    return <span className={`pill ${cls}`}>{t(`publish.status${s.charAt(0).toUpperCase()}${s.slice(1)}`)}</span>;
  };

  const open = KEY_PROVIDERS.find((p) => p.id === keyProvider);

  const connectGrid = (
    <>
      <div className="pick-grid">
        {PROVIDERS.map((p) => (
          <button key={p.id} className="pick" onClick={() => connect(p.id)} disabled={busy || !inTauri}>
            <span className="pick-ico"><Mark id={p.id} /></span>
            <span>
              <span className="pick-title">{p.label}</span>
              <span className="pick-sub">{t("publish.connectVia")}</span>
            </span>
          </button>
        ))}
        {DEVICE_PROVIDERS.map((p) => (
          <button
            key={p.id}
            className={`pick ${deviceCode?.provider === p.id ? "active" : ""}`}
            onClick={() => connectDevice(p.id)}
            disabled={busy || !inTauri}
          >
            <span className="pick-ico"><Mark id={p.id} /></span>
            <span>
              <span className="pick-title">{p.label}</span>
              <span className="pick-sub">{t("publish.connectViaCode")}</span>
            </span>
          </button>
        ))}
        {KEY_PROVIDERS.map((p) => (
          <button
            key={p.id}
            className={`pick ${keyProvider === p.id ? "active" : ""}`}
            onClick={() => { setKeyProvider(keyProvider === p.id ? "" : p.id); setKeyDraft(""); setKeyLabel(""); }}
            disabled={busy || !inTauri}
          >
            <span className="pick-ico"><Mark id={p.id} /></span>
            <span>
              <span className="pick-title">{p.label}</span>
              <span className="pick-sub">{t("publish.connectViaKey")}</span>
            </span>
          </button>
        ))}
      </div>

      {/* Offered in the desktop shell too. Its callback lands on the same
          machine as the daemon so it usually arrives on its own — but "usually"
          is not "always" (a blocked loopback port, a browser that swallowed the
          redirect), and a login with no way to finish it is a dead end. Only the
          reason it can fail differs, so only the hint does. */}
      {pasteFlow && (
        <div className="keyform fade-in">
          <div className="callout info">
            <IconInfo />
            <span>{t(realTauri ? "publish.pasteHintDesktop" : "publish.pasteHint")}</span>
          </div>
          {/* The callback URL is long, so it gets the full row and the submit
              button sits beside it rather than on a line of its own. */}
          <div className="field">
            <label htmlFor="oauthpaste">{t("publish.pasteLabel")}</label>
            <div className="paste-row">
              <input
                id="oauthpaste"
                className="input mono"
                autoComplete="off"
                spellCheck={false}
                value={pasteDraft}
                placeholder={t("publish.pastePlaceholder")}
                onChange={(e) => setPasteDraft(e.target.value)}
                onKeyDown={(e) => { if (e.key === "Enter" && pasteDraft.trim()) submitPastedCode(); }}
              />
              <button className="btn" onClick={submitPastedCode} disabled={!pasteDraft.trim()}>
                {t("publish.pasteSubmit")}
              </button>
            </div>
          </div>
        </div>
      )}

      {deviceCode && (
        <div className="keyform fade-in">
          <div className="callout info">
            <IconInfo />
            <span>
              {t("publish.deviceHint")}{" "}
              <a href={deviceCode.url} target="_blank" rel="noreferrer">{deviceCode.url}</a>
            </span>
          </div>
          {deviceCode.code && (
            <div className="devicecode">
              <span className="devicecode-label">{t("publish.deviceCode")}</span>
              <CopyChip value={deviceCode.code} />
            </div>
          )}
          <p className="muted">{t("publish.deviceWaiting")}</p>
        </div>
      )}

      {open && (
        <div className="keyform fade-in">
          <div className="callout info">
            <IconInfo />
            <span>
              {t("publish.keyHint")}{" "}
              <a href={open.keyUrl} target="_blank" rel="noreferrer">{open.keyUrl}</a>
            </span>
          </div>
          <div className="field">
            <label htmlFor="apikey">{t("publish.keyLabel", { provider: open.label })}</label>
            <input
              id="apikey"
              className="input"
              type="password"
              autoComplete="off"
              spellCheck={false}
              value={keyDraft}
              placeholder={t("publish.keyPlaceholder")}
              onChange={(e) => setKeyDraft(e.target.value)}
              onKeyDown={(e) => { if (e.key === "Enter" && keyDraft.trim()) connectKey(); }}
            />
          </div>
          <div className="field">
            <label htmlFor="apikeylabel">{t("publish.keyName")}</label>
            <input
              id="apikeylabel"
              className="input"
              value={keyLabel}
              placeholder={t("publish.keyNamePlaceholder")}
              onChange={(e) => setKeyLabel(e.target.value)}
            />
          </div>
          <div className="keyform-actions">
            <button className="btn sm" onClick={connectKey} disabled={busy || !keyDraft.trim()}>
              {t("publish.keyConnect")}
            </button>
            <button className="btn sm ghost" onClick={() => setKeyProvider("")} disabled={busy}>
              {t("publish.keyCancel")}
            </button>
          </div>
        </div>
      )}
    </>
  );

  return (
    <div>
      <PageHead
        title={t("publish.title")}
        sub={t("publish.sub")}
        actions={
          <IconAction
            icon={<IconRefresh />}
            label={t("publish.cliRescanHint")}
            onClick={rescanCli}
            disabled={!inTauri || rescanning}
            spinning={rescanning}
          />
        }
      />

      <Err>{err}</Err>

      {/* Connecting comes first: with no account yet the rest of the page has
          nothing to show, and the empty state points "above" for OAuth. */}
      <Card
        icon={<IconPlus />}
        title={t("publish.connectTitle")}
        desc={t("publish.connectDesc")}
      >
        {connectGrid}
        <Ok>{msg}</Ok>
      </Card>

      <Card
        icon={<IconShield />}
        title={t("publish.accountsTitle")}
        desc={t("publish.accountsDesc")}
        right={<span className="count-chip">{acctLoading ? "—" : accounts.length}</span>}
      >
        {acctLoading ? (
          <SkeletonRows rows={2} />
        ) : accounts.length === 0 ? (
          <Empty
            icon={<IconChip />}
            title={t("publish.accountsEmptyTitle")}
            desc={t("publish.accountsEmpty")}
            action={
              <button className="btn sm ghost" onClick={rescanCli} disabled={!inTauri || rescanning}>
                <IconRefresh className={rescanning ? "spin" : undefined} />
                {rescanning ? t("publish.scanning") : t("publish.cliRescan")}
              </button>
            }
          />
        ) : (
          <div className="acct-list">
            {accounts.map((a) => {
              const k = keyOf(a);
              const limit = a.sell_daily_limit;
              // What actually stops this account is the plan's 5h rolling
              // window, so that is the bar: today's tokens against a
              // daily-equivalent that no upstream enforces only ever read as a
              // limit that isn't there. A metered key has no window at all —
              // its bar is the operator's own daily cap, or nothing.
              const windowed = !a.key_based && a.window_cap > 0;
              const used = windowed ? a.used_window : a.used_today;
              const denom = windowed ? a.window_cap : limit;
              const pct = denom > 0 ? (used / denom) * 100 : 0;
              const capPct = a.daily_cap > 0 && limit > 0 && !a.key_based
                ? Math.round((limit / a.daily_cap) * 100)
                : 0;
              // Every lane of this account, switched on or not: the ranking
              // chart below doubles as this subscription's price board, and
              // "what would I be selling, and at what discount" is a question
              // worth being able to answer *before* flipping the switch.
              const own = lanes.filter((l) => l.provider === a.provider && l.account_id === a.account_id);
              const floor = floorOf(a);
              // The daily cap is spent: `rebuild_pool` has already clamped this
              // account's quota to zero, so every one of its models is off the
              // market until the UTC rollover. That is a whole-subscription
              // stop, and it earns a line of its own rather than being left to
              // be inferred from an `exhausted` pill.
              const capSpent = limit > 0 && a.used_today >= limit;
              return (
                <div key={k} className={`acct ${a.sell_enabled ? "selling" : ""}`}>
                  <div className="acct-head">
                    <Mark id={a.provider} />
                    <div className="acct-id">
                      <div className="acct-name">
                        {a.account_id}
                        {a.plan && <span className="muted"> · {a.plan}</span>}
                      </div>
                      <div className="acct-meta">
                        <span className="mono">{a.provider}</span>
                        {statusPill(a.status)}
                        {a.shared_with_local_cli
                          ? <span className="pill warn" title={t("publish.sharedHint")}>{t("publish.sharedBadge")}</span>
                          : <span className="pill on" title={t("publish.ownedHint")}>{t("publish.ownedBadge")}</span>}
                      </div>
                    </div>
                    <div className="acct-actions">
                      <label className="switch" title={t("publish.sellSwitch")}>
                        <input
                          type="checkbox"
                          checked={a.sell_enabled}
                          onChange={(e) => setSell(a, e.target.checked)}
                          disabled={!inTauri || !!pending[k]}
                        />
                        <span className="track" />
                      </label>
                      <button className="icon-btn ghost-danger" onClick={() => removeAccount(a)} title={t("publish.remove")}>
                        <IconTrash />
                      </button>
                    </div>
                  </div>

                  {a.status === "expired" && (
                    <div className="callout warn compact">
                      <IconInfo /><span>{t("publish.expiredHint")}</span>
                    </div>
                  )}

                  {capSpent && (
                    <div className="callout warn compact">
                      <IconInfo /><span>{t("publish.limitReached", { limit: fmtTokens(limit) })}</span>
                    </div>
                  )}

                  {/* Throughput first — it is the number this whole page exists
                      to move. Against the 5h window for a subscription, against
                      the operator's daily cap for a key. */}
                  <div className="acct-usage">
                    <div className="au-head">
                      <span>{windowed ? t("publish.windowUsed") : t("publish.limitUsedToday")}</span>
                      <span className="mono tabular">
                        {fmtTokens(used)}
                        {denom > 0 && <span className="faint"> / {fmtTokens(denom)}</span>}
                        {denom > 0 && <span className="au-pct"> · {Math.round(Math.min(100, pct))}%</span>}
                      </span>
                    </div>
                    <div className="bar">
                      <span
                        className={pct >= 90 ? "danger" : pct >= 70 ? "warn" : ""}
                        style={{ width: `${Math.min(100, pct)}%`, minWidth: pct > 0 ? 3 : 0 }}
                      />
                    </div>
                  </div>

                  <div className="acct-grid">
                    {/* Daily cap: a value with a pencil, an input once clicked */}
                    <div className="field">
                      <label>{t("publish.limitLabel")}</label>
                      {limitEditing === k ? (
                        <div className="input-row">
                          <input
                            className="input mono"
                            type="number"
                            min={0}
                            autoFocus
                            value={limitDraft[k] ?? ""}
                            onChange={(e) => setLimitDraft((d) => ({ ...d, [k]: e.target.value }))}
                            onKeyDown={(e) => {
                              if (e.key === "Enter") saveLimit(a);
                              if (e.key === "Escape") setLimitEditing("");
                            }}
                            placeholder="0"
                          />
                          <span className="unit">{t("publish.unitTokensDay")}</span>
                          <button className="btn sm" onClick={() => saveLimit(a)} disabled={!inTauri || !!pending[k]}>
                            {t("publish.limitSave")}
                          </button>
                          <button className="btn sm ghost" onClick={() => setLimitEditing("")}>
                            {t("publish.limitCancel")}
                          </button>
                        </div>
                      ) : (
                        <div className="value-row">
                          {/* The unit rides on the number itself. "500K" next
                              to a percentage band is a quantity with no
                              dimension, and tokens-vs-dollars is exactly the
                              guess a seller must not have to make. */}
                          <span className="value-strong mono tabular">
                            {limit > 0 ? fmtTokens(limit) : t("publish.limitNoCap")}
                          </span>
                          {limit > 0 && <span className="unit">{t("publish.unitTokensDay")}</span>}
                          <button
                            className="icon-btn sm"
                            onClick={() => editLimit(a)}
                            disabled={!inTauri || !!pending[k]}
                            title={t("publish.limitEdit")}
                            aria-label={t("publish.limitEdit")}
                          >
                            <IconPencil />
                          </button>
                          {limitSaved === k && <span className="value-note ok">{t("publish.limitSaved")}</span>}
                        </div>
                      )}
                      {/* The unit now rides on the value itself, so the hint
                          carries the thing the number alone cannot say: how
                          much of the plan a cap that size actually is. */}
                      <div className="hint">
                        {limit > 0
                          ? (capPct > 0
                              ? <>{capPct}% {t("publish.ofSubscription")}</>
                              : t("publish.limitTokensPerDay"))
                          : a.key_based ? t("publish.limitUnlimitedKey") : t("publish.limitUnlimited")}
                      </div>

                      {/* The price floor, on the same pattern: the fraction of
                          list price this subscription is willing to trade at.
                          A model the market has pushed below it leaves the
                          market until it comes back — see the chart below. */}
                      <label className="acct-sub-label after">{t("publish.bandLabel")}</label>
                      {bandEditing === k ? (
                        <div className="band-edit">
                          <div className="input-row">
                            <span className="band-cap">≥</span>
                            <input
                              className="input mono band-input"
                              type="number"
                              min={RATIO_MIN}
                              max={RATIO_MAX}
                              autoFocus
                              aria-label={t("publish.bandMin")}
                              value={bandDraft[k] ?? ""}
                              onChange={(e) => setBandDraft((d) => ({ ...d, [k]: e.target.value }))}
                              onKeyDown={(e) => {
                                if (e.key === "Enter") saveBand(a);
                                if (e.key === "Escape") setBandEditing("");
                              }}
                              placeholder={String(RATIO_MIN)}
                            />
                            <span className="unit">%</span>
                          </div>
                          <div className="band-presets">
                            <span className="band-cap">{t("publish.bandQuick")}</span>
                            {BAND_PRESETS.map((preset) => (
                              <button
                                key={preset}
                                className={`chip${Number(bandDraft[k]) === preset ? " on" : ""}`}
                                onClick={() => setBandDraft((d) => ({ ...d, [k]: String(preset) }))}
                              >
                                {noFloor(preset) ? t("publish.bandNone") : `≥ ${preset}%`}
                              </button>
                            ))}
                          </div>
                          <div className="input-row">
                            <button className="btn sm" onClick={() => saveBand(a)} disabled={!inTauri || !!pending[k]}>
                              {t("publish.limitSave")}
                            </button>
                            <button className="btn sm ghost" onClick={() => setBandEditing("")}>
                              {t("publish.limitCancel")}
                            </button>
                          </div>
                        </div>
                      ) : (
                        <div className="value-row">
                          <span className="value-strong mono tabular">
                            {noFloor(floor) ? t("publish.bandNone") : `≥ ${floor}%`}
                          </span>
                          {!noFloor(floor) && (
                            <span className="unit">{t("publish.unitOfList")}</span>
                          )}
                          <button
                            className="icon-btn sm"
                            onClick={() => editBand(a)}
                            disabled={!inTauri || !!pending[k]}
                            title={t("publish.bandEdit")}
                            aria-label={t("publish.bandEdit")}
                          >
                            <IconPencil />
                          </button>
                          {bandSaved === k && <span className="value-note ok">{t("publish.limitSaved")}</span>}
                        </div>
                      )}
                      {/* Say what the setting *does*, in the same words the
                          chart below uses, rather than restating its units. */}
                      <div className="hint">
                        {noFloor(floor)
                          ? t("publish.bandHintOff")
                          : t("publish.bandHintFloor", { lo: floor })}
                      </div>
                    </div>

                    {/* No expiry here: the only timestamp we hold is the access
                        token's, which the daemon refreshes on its own — showing
                        it reads as "this subscription dies tonight". A
                        credential that really is dead says so in the status
                        pill and the note above. */}
                    <div className="fact-grid tight">
                      <div className="fact">
                        <span className="fact-k">{t("publish.usedTodayFact")}</span>
                        <span className="fact-v mono">{fmtTokens(a.used_today)}</span>
                      </div>
                      {windowed && (
                        <div className="fact">
                          <span className="fact-k">{t("publish.quotaLeft")}</span>
                          <span className="fact-v mono">{fmtTokens(a.quota_remaining)}</span>
                        </div>
                      )}
                    </div>
                  </div>

                  {/* Per-model price and state, then where the credential came
                      from. Both sit below a hairline: the chart answers "which
                      of my models is the market paying for" when that is the
                      question, and stays quiet the rest of the time. */}
                  {(own.length > 0 || a.sources.length > 0) && (
                    <div className="acct-detail">
                      {own.length > 0 && (
                        <DiscountRank
                          lanes={own}
                          floor={floor}
                          now={now}
                          onResume={resume}
                          resuming={resuming}
                        />
                      )}
                      {a.sources.length > 0 && (
                        <div className="ad-row">
                          <span className="meta-k">{t("publish.tokenSources")}</span>
                          <div className="ad-chips">
                            {a.sources.map((s, i) => (
                              <span key={s} className={`pill mono plain ${i === 0 ? "accent" : ""}`} title={s}>
                                <span>{shortSource(s)}</span>
                              </span>
                            ))}
                            {a.sources.length > 1 && (
                              <span className="faint ad-note" title={t("publish.mergedHint")}>
                                {t("publish.mergedBadge", { n: a.sources.length })}
                              </span>
                            )}
                          </div>
                        </div>
                      )}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
        <Ok>{importMsg}</Ok>
        {importWarnings.length > 0 && (
          <div className="callout warn card-foot">
            <IconShield /><span>{t("publish.envWarning", { vars: importWarnings.join(", ") })}</span>
          </div>
        )}
        {buyingTools.length > 0 && (
          <div className="callout card-foot">
            <IconInfo />
            <span>{t("publish.cliSkippedBuying", { tools: buyingTools.join(", ") })}</span>
          </div>
        )}
        <Err>{importErr}</Err>
        <Err>{acctErr}</Err>
      </Card>
    </div>
  );
}
