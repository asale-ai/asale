// Personal center (signed-in view): profile, security (password + linked
// accounts), preferences (language/theme), API key, sign-out.
import { useEffect, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { invoke, inTauri, runOAuthFlow, type Profile } from "../../lib";
import { LANGUAGES, setLanguage, type Language } from "../../i18n";
import { THEMES, useTheme, type Theme } from "../../theme";
import { Card, CopyChip, Ok, Err, Section } from "../../ui";
import { IconAccount, IconShield, IconSettings, IconKey, IconSun, IconMoon, IconGlobe, IconRefresh } from "../../icons";
import { GitHubIcon, GoogleIcon } from "./icons";

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
  const [theme, setTheme] = useTheme();

  const [name, setName] = useState(profile.name ?? "");
  const [region, setRegion] = useState(profile.region ?? "");
  const [profileMsg, setProfileMsg] = useState("");
  const [profileErr, setProfileErr] = useState("");

  const [oldPw, setOldPw] = useState("");
  const [newPw, setNewPw] = useState("");
  const [pwMsg, setPwMsg] = useState("");
  const [pwErr, setPwErr] = useState("");

  const [oauthBusy, setOauthBusy] = useState<string | null>(null);
  const [oauthMsg, setOauthMsg] = useState("");
  const [oauthErr, setOauthErr] = useState("");

  const [apiKey, setApiKey] = useState("");
  const [keyErr, setKeyErr] = useState("");
  const [keyBusy, setKeyBusy] = useState(false);

  // The asale key is provisioned automatically — fetch (minting if needed) on
  // open so the user never has to press a "generate" button.
  useEffect(() => {
    if (!inTauri) return;
    invoke<{ key: string | null }>("ensure_api_key")
      .then((r) => { if (r.key) setApiKey(r.key); })
      .catch((e) => setKeyErr(String((e as Error).message)));
  }, []);

  async function refresh() { onProfile(await invoke<Profile>("me_profile")); }

  async function saveProfile() {
    setProfileMsg(""); setProfileErr("");
    try {
      const p = await invoke<Profile>("update_profile", { name: name.trim() || undefined, region: region.trim() || undefined });
      onProfile(p); setProfileMsg(t("account.profile.saved"));
    } catch (e) { setProfileErr(String((e as Error).message)); }
  }

  async function updatePassword() {
    setPwMsg(""); setPwErr("");
    try {
      await invoke("change_password", { oldPassword: profile.has_password ? oldPw : undefined, newPassword: newPw });
      setOldPw(""); setNewPw(""); setPwMsg(t("account.security.passwordUpdated")); await refresh();
    } catch (e) { setPwErr(String((e as Error).message)); }
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

  // Mint a brand-new key, invalidating the previous one.
  async function regenKey() {
    setKeyErr(""); setKeyBusy(true);
    try { const r = await invoke<{ key: string }>("create_api_key", { label: "asale" }); setApiKey(r.key); }
    catch (e) { setKeyErr(String((e as Error).message)); } finally { setKeyBusy(false); }
  }

  const initial = (profile.name || profile.email || "?").trim().charAt(0).toUpperCase();
  const memberSince = profile.created_at ? new Date(profile.created_at).toLocaleDateString(i18n.language) : "—";

  return (
    <div>
      {/* Identity banner — the one place the account itself is the subject, so
          it gets a card of its own rather than a bare page title. */}
      <div className="profile-hero">
        {profile.avatar_url ? <img className="avatar" src={profile.avatar_url} alt="" /> : <div className="avatar">{initial}</div>}
        <div className="ph-id">
          <h1>{profile.name || profile.email}</h1>
          <p className="ph-mail">{profile.email}</p>
          <div className="ph-tags">
            <span className="pill plain">{t("account.profile.memberSince")} {memberSince}</span>
            {profile.kyc_level > 0 && <span className="pill on plain">KYC L{profile.kyc_level}</span>}
          </div>
        </div>
        <button className="btn ghost" onClick={onLogout}>{t("account.logout")}</button>
      </div>

      <Card icon={<IconAccount />} title={t("account.profile.title")}>
        <div className="field">
          <label>{t("account.profile.name")}</label>
          <input className="input" value={name} onChange={(e) => setName(e.target.value)} placeholder={profile.email} />
        </div>
        <div className="grid2">
          <div className="field">
            <label>{t("account.email")}</label>
            <input className="input" value={profile.email} readOnly disabled />
          </div>
          <div className="field">
            <label>{t("account.region")}</label>
            <input className="input" value={region} onChange={(e) => setRegion(e.target.value)} placeholder="—" />
          </div>
        </div>
        <button className="btn" onClick={saveProfile}>{t("account.profile.save")}</button>
        <Ok>{profileMsg}</Ok>
        <Err>{profileErr}</Err>
      </Card>

      <Card icon={<IconShield />} title={t("account.security.title")}>
        <Section
          first
          title={profile.has_password ? t("account.security.changePassword") : t("account.security.setPassword")}
          desc={profile.has_password ? undefined : t("account.security.setPasswordHint")}
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
          </div>
          <button className="btn ghost" onClick={updatePassword} disabled={!newPw}>{t("account.security.updatePassword")}</button>
          <Ok>{pwMsg}</Ok>
          <Err>{pwErr}</Err>
        </Section>

        <Section title={t("account.security.connectedAccounts")}>
          <div className="entity-list">
            {OAUTH_PROVIDERS.map(({ id, label, icon: Icon }) => {
              const linked = profile.oauth_accounts.find((a) => a.provider === id);
              return (
                <div className={`entity ${linked ? "is-on" : ""}`} key={id}>
                  <span className="e-badge lr-ico"><Icon size={20} /></span>
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
          {oauthBusy && <p className="muted" style={{ fontSize: 12.5, marginTop: 10 }}>{t("account.waitingAuth")}</p>}
          <Ok>{oauthMsg}</Ok>
          <Err>{oauthErr}</Err>
        </Section>
      </Card>

      <Card icon={<IconSettings />} title={t("account.preferences.title")}>
        <div className="field">
          <label>{t("account.preferences.language")}</label>
          <select className="input" value={i18n.language} onChange={(e) => setLanguage(e.target.value as Language)}>
            {LANGUAGES.map((l) => <option key={l.id} value={l.id}>{l.label}</option>)}
          </select>
        </div>
        <div className="field" style={{ marginBottom: 0 }}>
          <label>{t("account.preferences.theme")}</label>
          <div className="segmented">
            {THEMES.map((th) => (
              <button key={th} className={theme === th ? "active" : ""} onClick={() => setTheme(th)}>
                {THEME_ICON[th]}{t(`account.preferences.${th}`)}
              </button>
            ))}
          </div>
        </div>
      </Card>

      <Card icon={<IconKey />} title={t("account.apiKey.title")} desc={t("account.apiKey.desc")}>
        {apiKey
          ? <CopyChip value={apiKey} wrap />
          : <p className="muted" style={{ fontSize: 13, margin: 0 }}>{t("account.apiKey.provisioning")}</p>}
        <button className="btn sm subtle" style={{ marginTop: 12 }} onClick={regenKey} disabled={!inTauri || keyBusy}>
          <IconRefresh className={keyBusy ? "spin" : undefined} />{t("account.apiKey.regenerate")}
        </button>
        <Err>{keyErr}</Err>
      </Card>
    </div>
  );
}
