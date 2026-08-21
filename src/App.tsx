import { lazy, Suspense, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, inTauri, isDaemonDown, waitForDaemon, type Profile } from "./lib";
import {
  IconDashboard, IconPublish, IconConsume, IconWallet,
  IconRecords, IconUsage, IconGauge, IconAccount, IconSettings,
  IconGlobe, IconGithub, IconShare, IconKey,
} from "./icons";
import { openExternal } from "./shell";
import { SITE_URL, REPO_URL } from "./links";
import { StatusWidget } from "./components/StatusWidget";
import { UpgradeRequiredDialog, useUpgradeNotice } from "./components/UpgradeGate";
import { hasPendingUpdate, startUpdateWatcher, useUpdateState } from "./lib/updates";
import { Skeleton, PageSkeleton } from "./ui";
import type { JSX } from "react";

const Dashboard = lazy(() => import("./pages/Dashboard").then((m) => ({ default: m.Dashboard })));
const Publish = lazy(() => import("./pages/Publish").then((m) => ({ default: m.Publish })));
const Consume = lazy(() => import("./pages/Consume").then((m) => ({ default: m.Consume })));
const ApiKeys = lazy(() => import("./pages/ApiKeys").then((m) => ({ default: m.ApiKeys })));
const WalletPage = lazy(() => import("./pages/Wallet").then((m) => ({ default: m.WalletPage })));
const Records = lazy(() => import("./pages/Records").then((m) => ({ default: m.Records })));
const Usage = lazy(() => import("./pages/Usage").then((m) => ({ default: m.Usage })));
const Limits = lazy(() => import("./pages/Limits").then((m) => ({ default: m.Limits })));
const Account = lazy(() => import("./pages/Account").then((m) => ({ default: m.Account })));
const Settings = lazy(() => import("./pages/Settings").then((m) => ({ default: m.Settings })));
// Lazy for the same reason the pages are: the sheet carries fifteen brand
// marks and a QR encoder, and most sessions never open it.
const EarningsShareDialog = lazy(() =>
  import("./components/EarningsShareDialog").then((m) => ({ default: m.EarningsShareDialog })),
);

type Tab = "dashboard" | "publish" | "consume" | "apikeys" | "usage" | "limits" | "wallet" | "records" | "account" | "settings";

const ICONS: Record<Tab, JSX.Element> = {
  dashboard: <IconDashboard />,
  publish: <IconPublish />,
  consume: <IconConsume />,
  apikeys: <IconKey />,
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
  // API keys sit at the bottom with settings rather than among the two
  // switches. Calling the gateway from your own code is a way in that you set
  // up once and then leave alone — it belongs with the things you configure,
  // not with the two switches you flip every day.
  { items: ["apikeys", "settings"] },
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
  const [sharing, setSharing] = useState(false);

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

  // Has the platform stopped trading with this build? Gated on `booted` for the
  // same reason the profile poll is: the daemon is not answering before that.
  const upgrade = useUpgradeNotice(booted);

  // Is there a newer release? Unlike the banner above, this is nobody's problem
  // yet — it is a marker on the Settings item and nothing more. Started here
  // rather than from the Settings page because the point is to reach a user who
  // has no reason to open it; not gated on `booted` because the release feed is
  // asale.ai, not the daemon, and the check runs on its own clock anyway.
  useEffect(startUpdateWatcher, []);
  const update = useUpdateState();
  const updatePending = hasPendingUpdate(update);

  // Allow any page to request navigation (e.g. "manage limits" from Publish).
  useEffect(() => {
    const onNav = (e: Event) => {
      const target = (e as CustomEvent).detail as Tab;
      if (target) setTab(target);
    };
    window.addEventListener("asale:nav", onNav);
    return () => window.removeEventListener("asale:nav", onNav);
  }, []);

  const navBtn = (id: Tab) => {
    // Settings is the only item that ever carries a marker, and the only one
    // that could: it is where the update is installed from. The dot says "there
    // is something here"; the tooltip is what says what, because a dot on its
    // own is a puzzle rather than a notice.
    const flagged = id === "settings" && updatePending;
    return (
      <button
        key={id}
        className={`navitem ${tab === id ? "active" : ""}`}
        onClick={() => setTab(id)}
        title={flagged ? t("update.navHint", { version: update.latest }) : undefined}
      >
        {/* Fixed-width icon slot: nav glyphs are 17px but the logo mark and the
            user avatar are 26px, so without it the three label columns in the
            sidebar start at three different x. */}
        <span className="nav-ico">{ICONS[id]}</span>
        {t(`nav.${id}`)}
        {flagged && <span className="nav-dot" aria-label={t("update.navHint", { version: update.latest })} />}
      </button>
    );
  };

  const initial = (profile?.name || profile?.email || "?").trim().charAt(0).toUpperCase();
  // Until the profile call answers, the card is a placeholder: rendering the
  // signed-out state here would tell a signed-in user they are logged out.
  const userCard = profile === undefined ? (
    <div className="sidebar-user is-loading" aria-busy="true">
      <Skeleton w="var(--nav-ico-w)" h="var(--nav-ico-w)" r={999} />
      <span className="su-text">
        <Skeleton w="72%" h={11} style={{ marginBottom: "var(--s4)" }} />
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
      {/* Outside the page container and above everything: what it blocks is
          driven from several pages, and from the user's terminal besides — so
          there is no page this could sensibly belong to. */}
      {upgrade && <UpgradeRequiredDialog notice={upgrade} />}
      <aside className="sidebar">
        <div className="logo">
          <img className="logo-mark" src="/logo.svg" alt="Asale" />
          <span>Asale</span>
          {/* The product is pre-1.0 and says so where the name is, not buried in
              settings: the tooltip is what turns the mark into the thing a user
              needs when a number looks wrong — an address to write to. */}
          <span className="logo-badge beta" tabIndex={0} role="note" data-tip={t("nav.betaTip")}>
            beta
          </span>
          {/* The native title bar is hidden, so "Asale (Dev)" from
              tauri.dev.conf.json is never shown — this is what tells a dev
              instance apart from the installed release beside it. */}
          {import.meta.env.DEV && <span className="logo-badge">dev</span>}
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
        {/* `data-tauri-drag-region` only fires for the element it is on, so the
            strip is draggable while the buttons inside it stay clickable. It is
            what replaces the title bar removed in tauri.conf.json: without it
            a maximised window on macOS could only be moved by its top 28px. */}
        <div className="topbar" data-tauri-drag-region>
          <div className="topbar-inner">
            <div className="topbar-links">
              <button
                type="button"
                className="iconlink"
                onClick={() => openExternal(SITE_URL)}
                title={t("nav.site")}
                aria-label={t("nav.site")}
              >
                <IconGlobe />
              </button>
              <button
                type="button"
                className="iconlink"
                onClick={() => openExternal(REPO_URL)}
                title={t("nav.github")}
                aria-label={t("nav.github")}
              >
                <IconGithub />
              </button>
              {/* In the top bar rather than on the overview, because sharing is
                  not one page's business: the numbers on the card come from the
                  wallet and the ledger, and the reader may be on either. */}
              <button
                type="button"
                className="iconlink"
                onClick={() => setSharing(true)}
                title={t("share.open")}
                aria-label={t("share.open")}
              >
                <IconShare />
              </button>
            </div>
            <StatusWidget />
          </div>
        </div>
        {sharing && (
          <Suspense fallback={null}>
            <EarningsShareDialog onClose={() => setSharing(false)} />
          </Suspense>
        )}
        <div className="main-inner fade-in" key={booted ? tab : "boot"}>
          {!booted ? <PageSkeleton /> : (
            <Suspense fallback={<PageSkeleton />}>
              {/* The overview map calls out the reader's own country, and the
                  only place that is known is the profile polled above. */}
              {tab === "dashboard" && <Dashboard onNavigate={setTab} region={profile?.region ?? ""} />}
              {tab === "publish" && <Publish />}
              {tab === "consume" && <Consume />}
              {tab === "apikeys" && <ApiKeys />}
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
