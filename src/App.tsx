import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, inTauri, type Profile } from "./lib";
import { Dashboard } from "./pages/Dashboard";
import { Publish } from "./pages/Publish";
import { Consume } from "./pages/Consume";
import { WalletPage } from "./pages/Wallet";
import { Records } from "./pages/Records";
import { Usage } from "./pages/Usage";
import { Limits } from "./pages/Limits";
import { Account } from "./pages/Account";
import { Settings } from "./pages/Settings";
import {
  IconDashboard, IconPublish, IconConsume, IconWallet,
  IconRecords, IconUsage, IconGauge, IconAccount, IconSettings,
} from "./icons";
import { StatusWidget } from "./components/StatusWidget";
import type { JSX } from "react";

type Tab = "dashboard" | "publish" | "consume" | "usage" | "limits" | "wallet" | "records" | "account" | "settings";

const ICONS: Record<Tab, JSX.Element> = {
  dashboard: <IconDashboard />,
  publish: <IconPublish />,
  consume: <IconConsume />,
  usage: <IconUsage />,
  limits: <IconGauge />,
  wallet: <IconWallet />,
  records: <IconRecords />,
  account: <IconAccount />,
  settings: <IconSettings />,
};

// Grouped navigation. `null` = a spacer that pushes the rest to the bottom.
// `account` is not listed here — it is rendered as the user card at the very
// bottom of the sidebar (see App below).
const NAV: Array<{ label?: string; items: Tab[] } | "spacer"> = [
  { items: ["dashboard"] },
  { label: "groupTrade", items: ["publish", "consume"] },
  { label: "groupUsage", items: ["usage", "limits"] },
  { label: "groupFinance", items: ["wallet", "records"] },
  "spacer",
  { items: ["settings"] },
];

export function App() {
  const { t } = useTranslation();
  const [tab, setTab] = useState<Tab>("dashboard");
  const [profile, setProfile] = useState<Profile | null>(null);

  useEffect(() => {
    if (!inTauri) return;
    invoke<Profile>("me_profile").then(setProfile).catch(() => {});
    // Provision the asale API key automatically on launch (no-op if not yet
    // signed in; the account view provisions it again right after login).
    invoke("ensure_api_key").catch(() => {});
    // Keep the sidebar user card in sync after sign in / sign out.
    const poll = () => invoke<Profile>("me_profile").then(setProfile).catch(() => setProfile(null));
    poll();
    const id = setInterval(poll, 4000);
    return () => clearInterval(id);
  }, []);

  // Allow any page to request navigation (e.g. "manage limits" from Publish).
  useEffect(() => {
    const onNav = (e: Event) => {
      const target = (e as CustomEvent).detail as Tab;
      if (target) setTab(target);
    };
    window.addEventListener("asale:nav", onNav);
    return () => window.removeEventListener("asale:nav", onNav);
  }, []);

  const navBtn = (id: Tab) => (
    <button key={id} className={`navitem ${tab === id ? "active" : ""}`} onClick={() => setTab(id)}>
      {ICONS[id]}
      {t(`nav.${id}`)}
    </button>
  );

  const initial = (profile?.name || profile?.email || "?").trim().charAt(0).toUpperCase();
  const userCard = (
    <button
      className={`sidebar-user ${tab === "account" ? "active" : ""}`}
      onClick={() => setTab("account")}
      title={profile ? profile.email : t("nav.account")}
    >
      {profile?.avatar_url
        ? <img className="su-avatar" src={profile.avatar_url} alt="" />
        : profile
          ? <span className="su-avatar su-initial">{initial}</span>
          : <span className="su-avatar su-initial"><IconAccount /></span>}
      <span className="su-text">
        <span className="su-name">{profile ? (profile.name || profile.email) : t("account.signIn")}</span>
        <span className="su-sub">{profile ? profile.email : t("dashboard.signInPrompt")}</span>
      </span>
    </button>
  );

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="logo">
          <img className="logo-mark" src="/logo.svg" alt="Asale" />
          <span style={{ color: "var(--fg)" }}>Asale</span>
        </div>
        {NAV.map((g, i) =>
          g === "spacer" ? (
            <div key={i} style={{ flex: 1 }} />
          ) : (
            <div key={i}>
              {g.label && <div className="nav-group-label">{t(`nav.${g.label}`)}</div>}
              {g.items.map(navBtn)}
            </div>
          ),
        )}
        {userCard}
      </aside>
      <main className="main">
        {/* Outside the keyed page container on purpose: the status readout is
            global, so it must not remount (and re-poll from scratch) on every
            tab change. */}
        <div className="topbar">
          <div className="topbar-inner">
            <StatusWidget />
          </div>
        </div>
        <div className="main-inner fade-in" key={tab}>
          {tab === "dashboard" && <Dashboard onNavigate={setTab} />}
          {tab === "publish" && <Publish />}
          {tab === "consume" && <Consume />}
          {tab === "usage" && <Usage />}
          {tab === "limits" && <Limits />}
          {tab === "wallet" && <WalletPage />}
          {tab === "records" && <Records />}
          {tab === "account" && <Account />}
          {tab === "settings" && <Settings />}
        </div>
      </main>
    </div>
  );
}
