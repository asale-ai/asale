import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri, fmtUsdt, fmtTokens,
  type ProxyStatus, type Wallet,
  type AccountStatus, type BuyTool, type BuyTools,
} from "../lib";
import { Card, SkeletonRows, StatTile, PageHead, Empty, Mark } from "../ui";
import {
  IconWallet, IconPublish, IconConsume, IconArrowRight,
  IconChip, IconCheck, IconServer,
} from "../icons";

type Tab = "dashboard" | "publish" | "consume" | "usage" | "limits" | "wallet" | "records" | "account" | "settings";

const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);
const keyOf = (a: { provider: string; account_id: string }) => `${a.provider}:${a.account_id}`;

export function Dashboard({ onNavigate }: { onNavigate: (t: Tab) => void }) {
  const { t } = useTranslation();
  const [status, setStatus] = useState<ProxyStatus | null>(null);
  const [wallet, setWallet] = useState<Wallet | null>(null);
  const [accounts, setAccounts] = useState<AccountStatus[]>([]);
  const [tools, setTools] = useState<BuyTool[]>([]);
  const [loading, setLoading] = useState(true);
  const [daemonDown, setDaemonDown] = useState(false);

  useEffect(() => {
    if (!inTauri) { setLoading(false); return; }
    // One dropped poll is not an outage (the daemon restarts itself on update);
    // only a run of them earns the red banner.
    let fails = 0;
    const proxy = () =>
      invoke<ProxyStatus>("proxy_status")
        .then((s) => { fails = 0; setStatus(s); setDaemonDown(false); })
        .catch(() => { if (++fails >= 3) setDaemonDown(true); });
    const poll = () => {
      proxy();
      invoke<AccountStatus[]>("list_accounts").then(setAccounts).catch(() => {});
      invoke<BuyTools>("buy_tools").then((r) => setTools(r.tools || [])).catch(() => {});
      // The balance moves on its own as requests settle, and a failure at boot
      // must not leave the headline number blank until the next page visit.
      invoke<Wallet>("wallet_overview").then(setWallet).catch(() => {});
    };
    // Initial data load — shimmer until the first batch settles (polling below
    // refreshes silently and does not re-trigger the skeleton). `proxy_status`
    // is part of it: without it the sell tile would drop out of the skeleton
    // reading "offline" before the link state is actually known.
    Promise.allSettled([
      proxy(),
      invoke<Wallet>("wallet_overview").then(setWallet),
      invoke<AccountStatus[]>("list_accounts").then(setAccounts),
      invoke<BuyTools>("buy_tools").then((r) => setTools(r.tools || [])),
    ]).finally(() => setLoading(false));
    const id = setInterval(poll, 4000);
    return () => clearInterval(id);
  }, []);

  const ps = status?.publish_state ?? "offline";
  const online = ps === "online" || ps === "throttled";
  const connecting = ps === "connecting" || ps === "reconnecting";
  const sellLabel = t(`dashboard.${ps}`, { defaultValue: online ? t("dashboard.online") : t("dashboard.offline") });

  const selling = accounts.filter((a) => a.sell_enabled);
  const buying = tools.filter((x) => x.enabled);

  const statusPill = (s: AccountStatus["status"]) => {
    const cls = s === "available" ? "on" : s === "cooldown" || s === "exhausted" ? "warn" : "off";
    return <span className={`pill ${cls}`}>{t(`publish.status${cap(s)}`)}</span>;
  };

  const goto = (tab: Tab, label: string) => (
    <button className="btn sm ghost" onClick={() => onNavigate(tab)}>{label}<IconArrowRight /></button>
  );

  return (
    <div>
      <PageHead title={t("dashboard.title")} sub={t("dashboard.sub")} />

      {daemonDown && (
        <div className="callout danger card-lead">
          <IconServer /><span>{t("dashboard.daemonDown")}</span>
        </div>
      )}

      {/* Key numbers */}
      <div className="stat-grid">
        <StatTile
          primary
          loading={loading}
          icon={<IconWallet />}
          label={t("dashboard.walletBalance")}
          value={<span className="mono tabular">{wallet ? fmtUsdt(wallet.balance) : "—"}<span className="st-unit"> USDT</span></span>}
          sub={t("dashboard.walletSub")}
        />
        <StatTile
          loading={loading}
          icon={<IconPublish />}
          label={t("dashboard.publishing")}
          value={<><span className={`dot ${online ? "on" : connecting ? "warn pulse" : "off"}`} />{sellLabel}</>}
          sub={t("dashboard.sellAccountsCount", { on: selling.length, total: accounts.length })}
        />
        <StatTile
          loading={loading}
          icon={<IconConsume />}
          label={t("dashboard.buyState")}
          value={<><span className={`dot ${buying.length > 0 ? "on" : "off"}`} />{buying.length > 0 ? t("dashboard.buyingOn") : t("dashboard.buyingOff")}</>}
          sub={buying.length > 0 ? buying.map((x) => x.label).join(", ") : t("dashboard.buySubHint")}
        />
      </div>

      {/* Sell — per-account switches and today's progress */}
      <Card
        icon={<IconPublish />}
        title={t("dashboard.sellTitle")}
        desc={t("dashboard.sellDesc")}
        right={goto("publish", t("dashboard.goPublish"))}
      >
        {loading ? (
          <SkeletonRows rows={2} />
        ) : accounts.length === 0 ? (
          <Empty
            icon={<IconChip />}
            title={t("dashboard.noSellAccountsTitle")}
            desc={t("dashboard.noSellAccounts")}
            action={<button className="btn sm" onClick={() => onNavigate("publish")}>{t("dashboard.goConnect")}<IconArrowRight /></button>}
          />
        ) : (
          <div className="entity-list">
            {accounts.map((a) => {
              const denom = a.sell_daily_limit > 0 ? a.sell_daily_limit : a.daily_cap;
              const pct = denom > 0 ? Math.min(100, (a.used_today / denom) * 100) : 0;
              return (
                <div key={keyOf(a)} className={`entity ${a.sell_enabled ? "is-on" : "is-off"}`}>
                  <span className="e-badge"><Mark id={a.provider} /></span>
                  <div className="e-body">
                    <div className="e-title">
                      {a.account_id}
                      {a.plan && <span className="muted"> · {a.plan}</span>}
                    </div>
                    <div className="e-meta">
                      <span className="mono">{a.provider}</span>
                      <span className={`pill ${a.sell_enabled ? "on" : "off"}`}>
                        {a.sell_enabled ? t("dashboard.sellOn") : t("dashboard.sellOff")}
                      </span>
                      {a.sell_enabled && statusPill(a.status)}
                    </div>
                  </div>
                  <div className="e-num">
                    <span className="n-k">{t("dashboard.usedToday")}</span>
                    <span className="n-v mono tabular">
                      {fmtTokens(a.used_today)}{denom > 0 && <span className="faint"> / {fmtTokens(denom)}</span>}
                    </span>
                  </div>
                  {a.sell_enabled && denom > 0 && (
                    <div className="e-bar">
                      <div className="bar"><span style={{ width: `${pct}%`, minWidth: pct > 0 ? 3 : 0 }} /></div>
                      <span className="b-pct">{Math.round(pct)}%</span>
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </Card>

      {/* Buy — per-tool switches and model selections */}
      <Card
        icon={<IconConsume />}
        title={t("dashboard.buyTitle")}
        desc={t("dashboard.buyDesc")}
        right={goto("consume", t("dashboard.goConsume"))}
      >
        {loading ? (
          <SkeletonRows rows={2} />
        ) : (
          <div className="entity-list">
            {tools.map((tool) => (
              <div key={tool.id} className={`entity ${tool.enabled ? "is-on" : tool.installed ? "" : "is-off"}`}>
                <span className="e-badge"><Mark id={tool.id} /></span>
                <div className="e-body">
                  <div className="e-title">{tool.label}</div>
                  <div className="e-meta">
                    <span className={`pill ${tool.enabled ? "on" : "off"}`}>
                      {tool.enabled ? t("dashboard.buyOn") : t("dashboard.buyOff")}
                    </span>
                    {tool.enabled && tool.in_effect && (
                      <span className="pill on plain"><IconCheck /> {t("dashboard.inEffect")}</span>
                    )}
                    <span>{tool.installed ? (tool.account ?? t("dashboard.toolInstalled")) : t("dashboard.toolNotFound")}</span>
                  </div>
                </div>
                <div className="e-num wrap">
                  <span className="n-k">{t("dashboard.buyModels")}</span>
                  <span className="n-v mono">
                    {tool.models.length > 0 ? tool.models.join(", ") : t("dashboard.anyModel")}
                  </span>
                </div>
              </div>
            ))}
          </div>
        )}
      </Card>
    </div>
  );
}
