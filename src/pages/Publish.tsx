import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri, runOAuthFlow, fmtTokens,
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

const fmtTime = (secs: number | null) => (secs ? new Date(secs * 1000).toLocaleString() : "—");

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

  // The code a device-code login is waiting on, shown until it completes.
  const [deviceCode, setDeviceCode] = useState<{ provider: string; code: string; url: string } | null>(null);

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

  /** Flip one account's sell switch, or save its cap, via the same RPC. */
  async function setSell(a: AccountStatus, enabled: boolean, dailyLimit?: number) {
    const k = keyOf(a);
    setAcctErr("");
    setPending((p) => ({ ...p, [k]: true }));
    // Optimistic: the list refreshes on a 4s poll, too slow for a toggle.
    setAccounts((list) =>
      list.map((x) => (keyOf(x) === k
        ? { ...x, sell_enabled: enabled, sell_daily_limit: dailyLimit ?? x.sell_daily_limit }
        : x)),
    );
    try {
      await invoke("set_account_sell", {
        provider: a.provider,
        accountId: a.account_id,
        enabled,
        ...(dailyLimit === undefined ? {} : { dailyLimit }),
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
    await setSell(a, a.sell_enabled, dailyLimit);
    setLimitSaved(keyOf(a));
    setTimeout(() => setLimitSaved(""), 2000);
  }

  /** Open the cap editor on the value currently in force (so cancelling an edit
   *  and reopening never resumes a half-typed number). */
  function editLimit(a: AccountStatus) {
    const k = keyOf(a);
    setLimitDraft((d) => ({ ...d, [k]: String(a.sell_daily_limit || 0) }));
    setLimitEditing(k);
  }

  async function connect(provider: string) {
    setErr(""); setMsg(""); setBusy(true);
    try {
      // Two-step: the daemon opens/returns the authorize URL, we poll for the
      // loopback callback + token exchange to finish.
      const r = await runOAuthFlow<{ account_id: string }>("oauth_login", { provider });
      setMsg(t("publish.connected", { provider, account: r.account_id }));
      loadAccounts();
    } catch (e) { setErr(errText(e)); } finally { setBusy(false); }
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
              // Progress is measured against the cap when one is set, else
              // against the plan's daily-equivalent allowance.
              const denom = limit > 0 ? limit : a.daily_cap;
              const pct = denom > 0 ? (a.used_today / denom) * 100 : 0;
              const capPct = a.daily_cap > 0 && limit > 0 ? Math.round((limit / a.daily_cap) * 100) : 0;
              const own = a.sell_enabled
                ? lanes.filter((l) => l.provider === a.provider && l.account_id === a.account_id)
                : [];
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

                  {/* Today's throughput first — it is the number this whole
                      page exists to move. */}
                  <div className="acct-usage">
                    <div className="au-head">
                      <span>{t("publish.limitUsedToday")}</span>
                      <span className="mono tabular">
                        {fmtTokens(a.used_today)}
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
                          <button className="btn sm" onClick={() => saveLimit(a)} disabled={!inTauri || !!pending[k]}>
                            {t("publish.limitSave")}
                          </button>
                          <button className="btn sm ghost" onClick={() => setLimitEditing("")}>
                            {t("publish.limitCancel")}
                          </button>
                        </div>
                      ) : (
                        <div className="value-row">
                          <span className="value-strong mono tabular">
                            {limit > 0 ? fmtTokens(limit) : t("publish.limitNoCap")}
                          </span>
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
                      <div className="hint">
                        {limit > 0
                          ? <>{t("publish.limitTokensPerDay")}{capPct > 0 && <> · {capPct}% {t("publish.ofSubscription")}</>}</>
                          : t("publish.limitUnlimited")}
                      </div>
                    </div>

                    <div className="fact-grid tight">
                      <div className="fact">
                        <span className="fact-k">{t("publish.quotaLeft")}</span>
                        <span className="fact-v mono">{fmtTokens(a.quota_remaining)}</span>
                      </div>
                      <div className="fact">
                        <span className="fact-k">{t("publish.expires")}</span>
                        <span className="fact-v">{fmtTime(a.expires_at)}</span>
                      </div>
                    </div>
                  </div>

                  {/* Per-model state + where the credential came from. Both are
                      diagnostics, so they sit below a hairline: needed when
                      something is wrong, quiet the rest of the time. */}
                  {(own.length > 0 || a.sources.length > 0) && (
                    <div className="acct-detail">
                      {own.length > 0 && (
                        <div className="ad-row">
                          <span className="meta-k">{t("publish.lanes")}</span>
                          <div className="ad-chips">
                            {own.map((l) => {
                              const lk = `${l.provider}:${l.account_id}:${l.model}`;
                              const back = countdown(Math.max(l.resume_at, l.cooldown_until ?? 0), now);
                              const cls = l.status === "selling" ? "on" : l.requires_user ? "err" : "warn";
                              const why = l.paused_reason
                                ? t(`publish.lanePause.${l.paused_reason}`, { defaultValue: l.paused_reason })
                                : t(`publish.laneStatus.${l.status}`, { defaultValue: l.status });
                              return (
                                <span key={lk} className="lane" title={l.last_error || why}>
                                  <span className={`pill mono ${cls}`}>
                                    <span>{l.model}</span>
                                    {l.status !== "selling" && <span className="lane-why"> · {why}</span>}
                                    {back && <span className="lane-why"> · {back}</span>}
                                  </span>
                                  {l.requires_user && (
                                    <button
                                      className="lane-resume"
                                      onClick={() => resume(l)}
                                      disabled={!inTauri || !!resuming[lk]}
                                      title={t("publish.laneResumeHint")}
                                    >
                                      {t("publish.laneResume")}
                                    </button>
                                  )}
                                </span>
                              );
                            })}
                          </div>
                        </div>
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

      <Card
        icon={<IconPlus />}
        title={t("publish.connectTitle")}
        desc={t("publish.connectDesc")}
      >
        {connectGrid}
        <Ok>{msg}</Ok>
      </Card>
    </div>
  );
}
