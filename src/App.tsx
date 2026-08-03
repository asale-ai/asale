import { lazy, Suspense, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, inTauri, isDaemonDown, waitForDaemon, type Profile } from "./lib";
import {
  IconDashboard, IconPublish, IconConsume, IconWallet,
  IconRecords, IconUsage, IconGauge, IconAccount, IconSettings,
} from "./icons";
import { StatusWidget } from "./components/StatusWidget";
import { Skeleton, PageSkeleton } from "./ui";
import type { JSX } from "react";

const Dashboard = lazy(() => import("./pages/Dashboard").then((m) => ({ default: m.Dashboard })));
const Publish = lazy(() => import("./pages/Publish").then((m) => ({ default: m.Publish })));
const Consume = lazy(() => import("./pages/Consume").then((m) => ({ default: m.Consume })));
const WalletPage = lazy(() => import("./pages/Wallet").then((m) => ({ default: m.WalletPage })));
const Records = lazy(() => import("./pages/Records").then((m) => ({ default: m.Records })));
const Usage = lazy(() => import("./pages/Usage").then((m) => ({ default: m.Usage })));
const Limits = lazy(() => import("./pages/Limits").then((m) => ({ default: m.Limits })));
const Account = lazy(() => import("./pages/Account").then((m) => ({ default: m.Account })));
const Settings = lazy(() => import("./pages/Settings").then((m) => ({ default: m.Settings })));

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
  // `undefined` = not answered yet (show placeholders), `null` = signed out.
  const [profile, setProfile] = useState<Profile | null | undefined>(undefined);
  // The desktop shell starts its daemon in a background thread while this
  // webview is already painting, so the first RPCs of a healthy launch fail.
  // Pages mount only once the daemon has answered — otherwise every one of
  // them renders its "daemon down" / "signed out" branch for a second first.
  const [booted, setBooted] = useState(false);

  useEffect(() => {
    if (!inTauri) { setBooted(true); return; }
    let alive = true;
    // Resolves either way: if the daemon really never comes up, the pages must
    // still mount and say so (that is what the status widget is for).
    waitForDaemon().then(() => { if (alive) setBooted(true); });
    return () => { alive = false; };
  }, []);

  useEffect(() => {
    if (!inTauri || !booted) return;
    let alive = true;
    // Provision the asale API key automatically on launch (no-op if not yet
    // signed in; the account view provisions it again right after login).
    invoke("ensure_api_key").catch(() => {});
    // Keep the sidebar user card in sync after sign in / sign out. A dead
    // daemon is not a sign-out — leave the card as it was rather than
    // flipping it to "sign in" on a transient failure.
    const poll = () => invoke<Profile>("me_profile")
      .then((p) => { if (alive) setProfile(p); })
      .catch((e) => { if (alive && !isDaemonDown(e)) setProfile(null); });
    poll();
    const id = setInterval(poll, 4000);
    return () => { alive = false; clearInterval(id); };
  }, [booted]);

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
      {/* Fixed-width icon slot: nav glyphs are 17px but the logo mark and the
          user avatar are 26px, so without it the three label columns in the
          sidebar start at three different x. */}
      <span className="nav-ico">{ICONS[id]}</span>
      {t(`nav.${id}`)}
    </button>
  );

  const initial = (profile?.name || profile?.email || "?").trim().charAt(0).toUpperCase();
  // Until the profile call answers, the card is a placeholder: rendering the
  // signed-out state here would tell a signed-in user they are logged out.
  const userCard = profile === undefined ? (
    <div className="sidebar-user is-loading" aria-busy="true">
      <Skeleton w="var(--nav-ico-w)" h="var(--nav-ico-w)" r={999} />
      <span className="su-text">
        <Skeleton w="72%" h={11} style={{ marginBottom: 5 }} />
        <Skeleton w="52%" h={9} />
      </span>
    </div>
  ) : (
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
          <span>Asale</span>
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
        <div className="main-inner fade-in" key={booted ? tab : "boot"}>
          {!booted ? <PageSkeleton /> : (
            <Suspense fallback={<PageSkeleton />}>
              {/* The overview map calls out the reader's own country, and the
                  only place that is known is the profile polled above. */}
              {tab === "dashboard" && <Dashboard onNavigate={setTab} region={profile?.region ?? ""} />}
              {tab === "publish" && <Publish />}
              {tab === "consume" && <Consume />}
              {tab === "usage" && <Usage />}
              {tab === "limits" && <Limits />}
              {tab === "wallet" && <WalletPage />}
              {tab === "records" && <Records />}
              {tab === "account" && <Account />}
              {tab === "settings" && <Settings />}
            </Suspense>
          )}
        </div>
      </main>
    </div>
  );
}
