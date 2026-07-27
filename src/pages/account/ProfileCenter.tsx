// Personal center (signed-in view). Laid out like the web account page: a page
// head, one identity card, then three tabs — profile, security (password,
// linked accounts, API key) and preferences (language/theme).
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { countryOptions } from "@shared/countries";
import { invoke, inTauri, runOAuthFlow, type Profile } from "../../lib";
import { LANGUAGES, setLanguage, type Language } from "../../i18n";
import { THEMES, useTheme, type Theme } from "../../theme";
import { Card, CopyChip, Ok, Err, PageHead } from "../../ui";
import {
  IconAccount, IconShield, IconSettings, IconKey, IconLink, IconInfo,
  IconSun, IconMoon, IconGlobe, IconRefresh, IconPencil, IconCheck,
} from "../../icons";
import { GitHubIcon, GoogleIcon } from "./icons";

const TABS = ["profile", "security", "preferences"] as const;
type Tab = (typeof TABS)[number];

const TAB_ICON: Record<Tab, ReactNode> = {
  profile: <IconAccount />, security: <IconShield />, preferences: <IconSettings />,
};

const OAUTH_PROVIDERS = [
  { id: "google", label: "Google", icon: GoogleIcon },
  { id: "github", label: "GitHub", icon: GitHubIcon },
] as const;

const THEME_ICON: Record<Theme, ReactNode> = {
  light: <IconSun />, dark: <IconMoon />, system: <IconGlobe />,
};

interface Props { profile: Profile; onProfile: (p: Profile) => void; onLogout: () => void; }

export function ProfileCenter({ profile, onProfile, onLogout }: Props) {
  const { t, i18n } = useTranslation();
  const [tab, setTab] = useState<Tab>("profile");

  const initial = (profile.name || profile.email || "?").trim().charAt(0).toUpperCase();
  const memberSince = profile.created_at
    ? new Date(profile.created_at).toLocaleDateString(i18n.language)
    : "—";

  return (
    <div>
      <PageHead
        title={t("account.center.title")}
        sub={t("account.center.subtitle")}
        actions={<button className="btn sm ghost" onClick={onLogout}>{t("account.logout")}</button>}
      />

      {/* Identity card — the one place the account itself is the subject. */}
      <div className="profile-hero">
        {profile.avatar_url
          ? <img className="avatar" src={profile.avatar_url} alt="" />
          : <div className="avatar">{initial}</div>}
        <div className="ph-id">
          {/* Without a display name the title already *is* the e-mail — don't repeat it. */}
          <h1>{profile.name || profile.email}</h1>
          {profile.name && <p className="ph-mail">{profile.email}</p>}
          <div className="ph-tags">
            <span className="pill plain">KYC L{profile.kyc_level}</span>
            <span className="pill plain">ID {profile.user_id}</span>
            <span className="pill plain">{t("account.profile.memberSince")} {memberSince}</span>
          </div>
        </div>
      </div>

      <div className="tabs">
        {TABS.map((v) => (
          <button key={v} className={tab === v ? "active" : ""} onClick={() => setTab(v)}>
            {TAB_ICON[v]}{t(`account.center.tab.${v}`)}
          </button>
        ))}
      </div>

      {tab === "profile" && <ProfileTab profile={profile} onProfile={onProfile} />}
      {tab === "security" && <SecurityTab profile={profile} onProfile={onProfile} />}
      {tab === "preferences" && <PreferencesTab />}
    </div>
  );
}

// ── Profile ─────────────────────────────────────────────────────────────────

function ProfileTab({ profile, onProfile }: { profile: Profile; onProfile: (p: Profile) => void }) {
  const { t, i18n } = useTranslation();
  const countries = useMemo(() => countryOptions(i18n.language), [i18n.language]);
  const countryName = useMemo(() => new Map(countries.map((c) => [c.code, c.name])), [countries]);
  const memberSince = profile.created_at
    ? new Date(profile.created_at).toLocaleDateString(i18n.language)
    : "—";

  async function save(patch: Record<string, string>) {
    onProfile(await invoke<Profile>("update_profile", patch));
  }

  return (
    <div className="fade-in">
      <Card icon={<IconAccount />} title={t("account.profile.basics")} desc={t("account.profile.basicsDesc")}>
        <div className="ifields">
          <InlineField
            label={t("account.profile.name")}
            hint={t("account.profile.nameDesc")}
            value={profile.name}
            onSave={(v) => save({ name: v })}
          />
          <InlineField
            label={t("account.profile.avatarUrl")}
            hint={t("account.profile.avatarUrlDesc")}
            value={profile.avatar_url}
            placeholder="https://…"
            mono
            onSave={(v) => save({ avatar_url: v })}
          />
          <InlineField
            label={t("account.region")}
            hint={t("account.profile.regionDesc")}
            value={profile.region}
            placeholder={t("account.regionPlaceholder")}
            /* A picker, not a text box: the server takes ISO codes only, and the
               world map counts one country per exact code. */
            options={countries.map((c) => ({ value: c.code, label: c.name }))}
            render={(code) => countryName.get(code) ?? code}
            onSave={(v) => save({ region: v })}
          />
        </div>
      </Card>

      <Card icon={<IconInfo />} title={t("account.profile.accountInfo")}>
        <dl className="drows">
          <div className="drow"><dt>{t("account.email")}</dt><dd>{profile.email}</dd></div>
          <div className="drow"><dt>{t("account.profile.userId")}</dt><dd className="tabular">{profile.user_id}</dd></div>
          <div className="drow"><dt>{t("account.profile.memberSince")}</dt><dd>{memberSince}</dd></div>
          <div className="drow"><dt>{t("account.profile.kycLevel")}</dt><dd><span className="pill plain">L{profile.kyc_level}</span></dd></div>
          <div className="drow">
            <dt>{t("account.profile.status")}</dt>
            <dd>
              <span className={`pill ${profile.status === 1 ? "on" : "off"}`}>
                {profile.status === 1
                  ? t("account.profile.statusActive")
                  : t("account.profile.statusOther", { code: profile.status })}
              </span>
            </dd>
          </div>
        </dl>
      </Card>
    </div>
  );
}

/** A read-only row that turns into an input on demand. */
function InlineField({
  label, hint, value, placeholder, mono, options, render, onSave,
}: {
  label: string;
  hint: string;
  value: string;
  placeholder?: string;
  mono?: boolean;
  /** Turns the editor into a picker (the country, which the server accepts
   *  only as an ISO code — a text box invited typos it then rejected). */
  options?: { value: string; label: string }[];
  /** How to show the stored value when not editing (a code as its country name). */
  render?: (value: string) => string;
  onSave: (value: string) => Promise<void>;
}) {
  const { t } = useTranslation();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(value);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const [err, setErr] = useState("");

  // A save elsewhere (or a reload) should win over a stale draft. Adjusting
  // during render rather than in an effect avoids a second render pass.
  const [syncedFrom, setSyncedFrom] = useState(value);
  if (syncedFrom !== value) {
    setSyncedFrom(value);
    setDraft(value);
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true); setErr("");
    try {
      await onSave(draft.trim());
      setEditing(false);
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch (e2) {
      setErr(String((e2 as Error).message));
    } finally {
      setBusy(false);
    }
  }

  function cancel() {
    setDraft(value); setErr(""); setEditing(false);
  }

  return (
    <div className="ifield">
      <div className="if-head">
        <div className="if-main">
          <div className="if-label">
            {label}
            {saved && (
              <span className="pill on plain tiny fade-in">
                <IconCheck />{t("account.profile.saved")}
              </span>
            )}
          </div>
          {!editing && (
            <div className={`if-value${value ? "" : " unset"}${mono && value ? " mono" : ""}`}>
              {value ? (render ? render(value) : value) : t("account.profile.notSet")}
            </div>
          )}
          <p className="if-hint">{hint}</p>
        </div>
        {!editing && (
          <button className="btn sm ghost" onClick={() => setEditing(true)}>
            <IconPencil />{t("account.profile.edit")}
          </button>
        )}
      </div>

      {editing && (
        <form className="if-edit fade-in" onSubmit={submit}>
          {options ? (
            <select className="input" value={draft} autoFocus onChange={(e) => setDraft(e.target.value)}>
              <option value="">{placeholder ?? t("account.profile.notSet")}</option>
              {options.map((o) => <option key={o.value} value={o.value}>{o.label}</option>)}
            </select>
          ) : (
            <input
              className={`input${mono ? " mono" : ""}`}
              value={draft}
              placeholder={placeholder}
              autoFocus
              onChange={(e) => setDraft(e.target.value)}
            />
          )}
          <button className="btn sm" disabled={busy}>{busy ? "…" : t("account.profile.save")}</button>
          <button type="button" className="btn sm ghost" onClick={cancel} disabled={busy}>
            {t("account.profile.cancel")}
          </button>
        </form>
      )}
      <Err>{err}</Err>
    </div>
  );
}

// ── Security ────────────────────────────────────────────────────────────────

function SecurityTab({ profile, onProfile }: { profile: Profile; onProfile: (p: Profile) => void }) {
  const { t } = useTranslation();

  const [oldPw, setOldPw] = useState("");
  const [newPw, setNewPw] = useState("");
  const [pwBusy, setPwBusy] = useState(false);
  const [pwMsg, setPwMsg] = useState("");
  const [pwErr, setPwErr] = useState("");

  const [oauthBusy, setOauthBusy] = useState<string | null>(null);
  const [oauthMsg, setOauthMsg] = useState("");
  const [oauthErr, setOauthErr] = useState("");

  const [apiKey, setApiKey] = useState("");
  const [keyErr, setKeyErr] = useState("");
  const [keyMsg, setKeyMsg] = useState("");
  const [keyBusy, setKeyBusy] = useState(false);
  // Regenerating revokes the key the buying tools are holding, so it asks first.
  const [keyConfirm, setKeyConfirm] = useState(false);

  // The asale key is provisioned automatically — fetch (minting if needed) on
  // open so the user never has to press a "generate" button.
  useEffect(() => {
    if (!inTauri) return;
    invoke<{ key: string | null }>("ensure_api_key")
      .then((r) => { if (r.key) setApiKey(r.key); })
      .catch((e) => setKeyErr(String((e as Error).message)));
  }, []);

  async function refresh() { onProfile(await invoke<Profile>("me_profile")); }

  async function updatePassword() {
    setPwMsg(""); setPwErr(""); setPwBusy(true);
    try {
      await invoke("change_password", { oldPassword: profile.has_password ? oldPw : undefined, newPassword: newPw });
      setOldPw(""); setNewPw(""); setPwMsg(t("account.security.passwordUpdated")); await refresh();
    } catch (e) { setPwErr(String((e as Error).message)); } finally { setPwBusy(false); }
  }

  async function link(provider: string) {
    setOauthMsg(""); setOauthErr(""); setOauthBusy(provider);
    try {
      const r = await runOAuthFlow<{ provider: string; email: string }>("platform_oauth_login", { provider, link: true });
      setOauthMsg(t("account.security.linked", { provider: r.provider, email: r.email })); await refresh();
    } catch (e) { setOauthErr(String((e as Error).message)); } finally { setOauthBusy(null); }
  }

  async function unlink(provider: string) {
    setOauthMsg(""); setOauthErr("");
    try { await invoke("unlink_oauth", { provider }); await refresh(); }
    catch (e) { setOauthErr(String((e as Error).message)); }
  }

  // Mint a brand-new key, invalidating the previous one. The daemon rewrites
  // the new key into every tool that is buying, and names them back here — they
  // still need a restart to pick it up.
  async function regenKey() {
    setKeyErr(""); setKeyMsg(""); setKeyBusy(true); setKeyConfirm(false);
    try {
      const r = await invoke<{ key: string; refreshed_tools?: string[] }>("create_api_key", { label: "asale" });
      setApiKey(r.key);
      const tools = r.refreshed_tools ?? [];
      setKeyMsg(tools.length ? t("account.apiKey.refreshed", { tools: tools.join("、") }) : t("account.apiKey.regenerated"));
    }
    catch (e) { setKeyErr(String((e as Error).message)); } finally { setKeyBusy(false); }
  }

  return (
    <div className="fade-in">
      <Card
        icon={<IconShield />}
        title={profile.has_password ? t("account.security.changePassword") : t("account.security.setPassword")}
        desc={profile.has_password ? t("account.security.changePasswordDesc") : t("account.security.setPasswordHint")}
      >
        {profile.has_password && (
          <div className="field">
            <label>{t("account.security.oldPassword")}</label>
            <input className="input" type="password" value={oldPw} onChange={(e) => setOldPw(e.target.value)} autoComplete="current-password" />
          </div>
        )}
        <div className="field">
          <label>{t("account.security.newPassword")}</label>
          <input className="input" type="password" value={newPw} onChange={(e) => setNewPw(e.target.value)} autoComplete="new-password" />
          <span className="hint">{t("account.security.passwordHint")}</span>
        </div>
        <button className="btn ghost" onClick={updatePassword} disabled={!newPw || pwBusy}>
          {pwBusy ? "…" : t("account.security.updatePassword")}
        </button>
        <Ok>{pwMsg}</Ok>
        <Err>{pwErr}</Err>
      </Card>

      <Card
        icon={<IconLink />}
        title={t("account.security.connectedAccounts")}
        desc={t("account.security.connectedAccountsDesc")}
      >
        <div className="entity-list">
          {OAUTH_PROVIDERS.map(({ id, label, icon: Icon }) => {
            const linked = profile.oauth_accounts.find((a) => a.provider === id);
            return (
              <div className={`entity ${linked ? "is-on" : ""}`} key={id}>
                <span className="e-badge"><span className="mark md"><Icon size={17} /></span></span>
                <div className="e-body">
                  <div className="e-title">{label}</div>
                  <div className="e-meta">{linked ? linked.email : t("account.security.notLinked")}</div>
                </div>
                {linked
                  ? <button className="btn sm ghost" onClick={() => unlink(id)}>{t("account.security.unlink")}</button>
                  : <button className="btn sm ghost" onClick={() => link(id)} disabled={oauthBusy !== null}>{t("account.security.link")}</button>}
              </div>
            );
          })}
        </div>
        {oauthBusy && <p className="micro-label card-foot">{t("account.waitingAuth")}</p>}
        <Ok>{oauthMsg}</Ok>
        <Err>{oauthErr}</Err>
      </Card>

      <Card icon={<IconKey />} title={t("account.apiKey.title")} desc={t("account.apiKey.desc")}>
        {apiKey
          ? <CopyChip value={apiKey} wrap />
          : <p className="micro-label">{t("account.apiKey.provisioning")}</p>}
        {keyConfirm
          ? (
            <div className="card-foot">
              <p className="micro-label">{t("account.apiKey.confirmWarning")}</p>
              <div className="btn-row">
                <button className="btn sm subtle" onClick={regenKey} disabled={keyBusy}>
                  <IconRefresh className={keyBusy ? "spin" : undefined} />{t("account.apiKey.confirmRegenerate")}
                </button>
                <button className="btn sm ghost" onClick={() => setKeyConfirm(false)}>{t("account.profile.cancel")}</button>
              </div>
            </div>
          )
          : (
            <div className="btn-row card-foot">
              <button className="btn sm subtle" onClick={() => { setKeyMsg(""); setKeyErr(""); setKeyConfirm(true); }} disabled={!inTauri || keyBusy}>
                <IconRefresh className={keyBusy ? "spin" : undefined} />{t("account.apiKey.regenerate")}
              </button>
            </div>
          )}
        <Ok>{keyMsg}</Ok>
        <Err>{keyErr}</Err>
      </Card>
    </div>
  );
}

// ── Preferences ─────────────────────────────────────────────────────────────

function PreferencesTab() {
  const { t, i18n } = useTranslation();
  const [theme, setTheme] = useTheme();

  return (
    <Card
      icon={<IconSettings />}
      title={t("account.preferences.title")}
      desc={t("account.preferences.desc")}
      className="fade-in"
    >
      <div className="pref-rows">
        <div className="pref-row">
          <div className="pr-main">
            <div className="pr-title">{t("account.preferences.language")}</div>
            <p className="pr-desc">{t("account.preferences.languageDesc")}</p>
          </div>
          <select className="input" value={i18n.language} onChange={(e) => setLanguage(e.target.value as Language)}>
            {LANGUAGES.map((l) => <option key={l.id} value={l.id}>{l.label}</option>)}
          </select>
        </div>

        <div className="pref-row">
          <div className="pr-main">
            <div className="pr-title">{t("account.preferences.theme")}</div>
            <p className="pr-desc">{t("account.preferences.themeDesc")}</p>
          </div>
          <div className="segmented">
            {THEMES.map((th) => (
              <button key={th} className={theme === th ? "active" : ""} onClick={() => setTheme(th)}>
                {THEME_ICON[th]}{t(`account.preferences.${th}`)}
              </button>
            ))}
          </div>
        </div>
      </div>
    </Card>
  );
}
