import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri, fmtUsdt, fmtTokens,
  type ProxyStatus, type Wallet, type UsageSummary,
  type AccountStatus, type BuyTool, type BuyTools,
} from "../lib";
import { StatTile, PageHead, Mark } from "../ui";
import { FeaturedPrices } from "../components/FeaturedPrices";
import { WorldMap } from "../components/WorldMap";
import { IconWallet, IconPublish, IconConsume, IconArrowRight, IconServer } from "../icons";

type Tab = "dashboard" | "publish" | "consume" | "usage" | "limits" | "wallet" | "records" | "account" | "settings";

const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);
const keyOf = (a: { provider: string; account_id: string }) => `${a.provider}:${a.account_id}`;

/** `region` is the signed-in user's declared country ("" when signed out or
 *  never set); the map calls it out on the world. */
export function Dashboard({ onNavigate, region = "" }: { onNavigate: (t: Tab) => void; region?: string }) {
  const { t } = useTranslation();
  const [status, setStatus] = useState<ProxyStatus | null>(null);
  const [wallet, setWallet] = useState<Wallet | null>(null);
  const [accounts, setAccounts] = useState<AccountStatus[]>([]);
  const [tools, setTools] = useState<BuyTool[]>([]);
  const [earned, setEarned] = useState<number | null>(null);
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
    // What this device earned today, in USDT. It comes from the settled ledger
    // rather than from `used_today × price`: the amount on a provider record is
    // the server's own `provider_income` (fee already taken), so it is the money
    // and not an estimate of it.
    const sold = () =>
      invoke<UsageSummary>("usage_summary", { period: "day" }).then((s) => setEarned(s.sold.amount_usdt));
    const poll = () => {
      proxy();
      invoke<AccountStatus[]>("list_accounts").then(setAccounts).catch(() => {});
      invoke<BuyTools>("buy_tools").then((r) => setTools(r.tools || [])).catch(() => {});
      // The balance moves on its own as requests settle, and a failure at boot
      // must not leave the headline number blank until the next page visit.
      invoke<Wallet>("wallet_overview").then(setWallet).catch(() => {});
      sold().catch(() => {});
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
      sold(),
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

  const goto = (tab: Tab, label: string) => (
    <button className="btn sm ghost" onClick={() => onNavigate(tab)}>{label}<IconArrowRight /></button>
  );

  /** On or off, for the tile's corner: the dot carries the state, the word
   *  names it. */
  const state = (dot: string, label: string) => (
    <span className="pill plain"><span className={`dot ${dot}`} />{label}</span>
  );

  /** One mark per connected thing — the overview's answer to "what is there,
   *  and is it on". Identity is the mark, state is the dot on it, and the full
   *  rows this replaces are one click away on the page the tile links to. The
   *  title carries the detail for the one item the reader is pointing at. */
  const chip = (key: string, id: string, on: boolean, dim: boolean, dot: string, title: string) => (
    <span key={key} className={`mark-chip${on ? " is-on" : ""}${dim ? " is-off" : ""}`} title={title}>
      <Mark id={id} size="sm" />
      <span className={`mc-dot dot ${dot}`} />
    </span>
  );

  return (
    <div>
      <PageHead title={t("dashboard.title")} sub={t("dashboard.sub")} />

      {daemonDown && (
        <div className="callout danger card-lead">
          <IconServer /><span>{t("dashboard.daemonDown")}</span>
        </div>
      )}

      {/* Three tiles, and that is the whole state of the account: what the
          wallet holds, what is selling, what is buying. Each used to be a tile
          here *and* a card below repeating it; each pair is now one tile — on or
          off in the corner, what is connected in the middle. */}
      <div className="stat-grid">
        <StatTile
          primary
          loading={loading}
          icon={<IconWallet />}
          label={t("dashboard.walletBalance")}
          value={<span className="mono tabular">{wallet ? fmtUsdt(wallet.balance) : "—"}<span className="st-unit"> USDT</span></span>}
          // What today added to that balance, under the balance itself. Before a
          // first sale there is nothing to report, and a permanent "+0.00" would
          // read as a broken counter to everyone who only buys.
          sub={earned && earned > 0
            ? t("dashboard.earnedToday", { amount: fmtUsdt(earned) })
            : t("dashboard.walletSub")}
          foot={goto("wallet", t("dashboard.goWallet"))}
        />

        {/* Sell — link state, and one mark per connected subscription account */}
        <StatTile
          loading={loading}
          icon={<IconPublish />}
          label={t("dashboard.publishing")}
          state={state(online ? "on" : connecting ? "warn pulse" : "off", sellLabel)}
          value={accounts.length === 0 ? <span className="faint">—</span> : (
            <div className="mark-row">
              {accounts.map((a) =>
                chip(
                  keyOf(a),
                  a.provider,
                  a.sell_enabled,
                  !a.sell_enabled,
                  // An account that is selling still has a state of its own —
                  // cooling down or out of quota is not the same as serving.
                  a.sell_enabled ? (a.status === "available" ? "on" : "warn") : "off",
                  [
                    a.account_id + (a.plan ? ` · ${a.plan}` : ""),
                    a.sell_enabled ? t(`publish.status${cap(a.status)}`) : t("dashboard.sellOff"),
                    `${t("dashboard.usedToday")} ${fmtTokens(a.used_today)}`,
                  ].join(" — "),
                )
              )}
            </div>
          )}
          sub={accounts.length === 0
            ? t("dashboard.noSellAccounts")
            : t("dashboard.sellAccountsCount", { on: selling.length, total: accounts.length })}
          foot={accounts.length === 0
            ? goto("publish", t("dashboard.goConnect"))
            : goto("publish", t("dashboard.goPublish"))}
        />

        {/* Buy — one mark per CLI that can route through Asale */}
        <StatTile
          loading={loading}
          icon={<IconConsume />}
          label={t("dashboard.buyState")}
          state={state(
            buying.length > 0 ? "on" : "off",
            buying.length > 0 ? t("dashboard.buyingOn") : t("dashboard.buyingOff"),
          )}
          value={
            <div className="mark-row">
              {tools.map((tool) =>
                chip(
                  tool.id,
                  tool.id,
                  tool.enabled,
                  !tool.installed,
                  // On but not in effect means the config has not been rewritten
                  // yet — the switch says yes and the CLI has not been told.
                  tool.enabled ? (tool.in_effect ? "on" : "warn") : "off",
                  [
                    tool.label,
                    tool.enabled ? (tool.in_effect ? t("dashboard.inEffect") : t("dashboard.buyOn")) : t("dashboard.buyOff"),
                    tool.installed ? (tool.account ?? t("dashboard.toolInstalled")) : t("dashboard.toolNotFound"),
                    `${t("dashboard.buyModels")}: ${tool.models.length > 0 ? tool.models.join(", ") : t("dashboard.anyModel")}`,
                  ].join(" — "),
                )
              )}
            </div>
          }
          sub={buying.length > 0 ? buying.map((x) => x.label).join(", ") : t("dashboard.buySubHint")}
          foot={goto("consume", t("dashboard.goConsume"))}
        />
      </div>

      {/* One panel for the market: where the network is, and what it charges.
          The prices used to be a card of their own above the map, which made
          two blocks out of one statement — and gave four models three lines
          each to say what a name, a share and a curve say in one row. */}
      <WorldMap region={region}>
        <FeaturedPrices />
      </WorldMap>
    </div>
  );
}
