import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri, runOAuthFlow, fmtTokens,
  type AccountStatus, type ImportAllResult, type Lane,
} from "../lib";
import { Card, Ok, Err, SkeletonRows, PageHead, IconAction, Empty } from "../ui";
import { IconTrash, IconShield, IconChip, IconRefresh, IconPlus, IconPencil } from "../icons";

const PROVIDERS = [
  { id: "claude", label: "Claude Code", badge: "C", color: "#d97757" },
  { id: "claude_work", label: "Claude Work", badge: "C", color: "#b45309" },
  { id: "codex", label: "Codex / OpenAI", badge: "O", color: "#10a37f" },
  { id: "gemini", label: "Gemini", badge: "G", color: "#4285f4" },
];
const providerColor = (id: string) => PROVIDERS.find((p) => id.startsWith(p.id))?.color ?? "var(--accent)";
const providerBadge = (id: string) => (PROVIDERS.find((p) => id.startsWith(p.id))?.badge ?? id.charAt(0).toUpperCase());

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

function Badge({ id, size = 36 }: { id: string; size?: number }) {
  return (
    <span style={{
      width: size, height: size, borderRadius: size * 0.28, flexShrink: 0,
      display: "grid", placeItems: "center", background: providerColor(id), color: "#fff",
      fontSize: size * 0.44, fontWeight: 800,
    }}>{providerBadge(id)}</span>
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

  // Local-CLI import: the daemon runs it at startup, this only re-runs it.
  const [rescanning, setRescanning] = useState(false);
  const [importMsg, setImportMsg] = useState("");
  const [importWarnings, setImportWarnings] = useState<string[]>([]);
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
      .catch((e) => setAcctErr(String((e as Error).message)))
      .finally(() => setAcctLoading(false));
    invoke<{ lanes: Lane[] }>("list_lanes")
      .then((r) => setLanes(r.lanes))
      .catch(() => {});
  }, []);

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
      setAcctErr(String((e as Error).message));
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
      setAcctErr(String((e as Error).message));
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
    } catch (e) { setErr(String((e as Error).message)); } finally { setBusy(false); }
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
      if (r.errors.length > 0) setImportErr(r.errors.map((e) => `${e.provider}: ${e.error}`).join("; "));
      loadAccounts();
    } catch (e) { setImportErr(String((e as Error).message)); } finally { setRescanning(false); }
  }

  async function removeAccount(a: AccountStatus) {
    setAcctErr("");
    try { await invoke<boolean>("remove_account", { provider: a.provider, accountId: a.account_id }); loadAccounts(); }
    catch (e) { setAcctErr(String((e as Error).message)); }
  }

  const statusPill = (s: AccountStatus["status"]) => {
    const cls = s === "available" ? "on" : s === "cooldown" || s === "exhausted" ? "warn" : "off";
    return <span className={`pill ${cls}`}>{t(`publish.status${s.charAt(0).toUpperCase()}${s.slice(1)}`)}</span>;
  };

  const connectGrid = (
    <div className="pick-grid">
      {PROVIDERS.map((p) => (
        <button key={p.id} className="pick" onClick={() => connect(p.id)} disabled={busy || !inTauri}>
          <span className="pick-ico" style={{ background: "transparent" }}><Badge id={p.id} size={34} /></span>
          <span>
            <span className="pick-title">{p.label}</span>
            <span className="pick-sub">{t("publish.connectVia")}</span>
          </span>
        </button>
      ))}
    </div>
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
        right={<span className="count-chip">{accounts.length}</span>}
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
                    <Badge id={a.provider} />
                    <div className="acct-id">
                      <div className="acct-name">
                        {a.account_id}
                        {a.plan && <span className="muted" style={{ fontWeight: 400 }}> · {a.plan}</span>}
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
                    <div className="field" style={{ marginBottom: 0 }}>
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
                              <span className="muted" style={{ fontSize: 11.5 }} title={t("publish.mergedHint")}>
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
          <div className="callout warn" style={{ marginTop: 10 }}>
            <IconShield /><span>{t("publish.envWarning", { vars: importWarnings.join(", ") })}</span>
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
