import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, inTauri, runOAuthFlow, type Profile } from "../lib";
import { ProfileCenter } from "./account/ProfileCenter";
import { GitHubIcon, GoogleIcon } from "./account/icons";
import { Err } from "../ui";

export function Account() {
  const { t } = useTranslation();
  const [mode, setMode] = useState<"login" | "register">("login");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [profile, setProfile] = useState<Profile | null>(null);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const [oauthBusy, setOauthBusy] = useState<string | null>(null);

  useEffect(() => {
    if (inTauri) invoke<Profile>("me_profile").then(setProfile).catch(() => {});
  }, []);

  async function loadProfile() {
    setProfile(await invoke<Profile>("me_profile"));
    // Ensure the account has an asale API key as soon as it signs in.
    invoke("ensure_api_key").catch(() => {});
  }

  async function submit() {
    setErr(""); setBusy(true);
    try {
      if (mode === "login") await invoke("login", { email, password });
      else await invoke("register", { email, password });
      await loadProfile();
    } catch (e) { setErr(String((e as Error).message)); } finally { setBusy(false); }
  }

  async function oauth(provider: "google" | "github") {
    setErr(""); setOauthBusy(provider);
    try { await runOAuthFlow("platform_oauth_login", { provider, link: false }); await loadProfile(); }
    catch (e) { setErr(String((e as Error).message)); } finally { setOauthBusy(null); }
  }

  async function logout() {
    try { await invoke("logout"); } catch { /* clear local state regardless */ }
    setProfile(null);
  }

  if (profile) return <ProfileCenter profile={profile} onProfile={setProfile} onLogout={logout} />;

  const disabled = busy || oauthBusy !== null || !inTauri;

  return (
    <div className="auth-wrap">
      <div className="auth-card">
        <div className="auth-head">
          <div className="logo">
            <img className="logo-mark" src="/logo.svg" alt="Asale" />
          </div>
          <h1>{mode === "login" ? t("account.signIn") : t("account.createAccount")}</h1>
          <p className="sub">{t("account.subSignedOut")}</p>
        </div>

        <div className="card auth-panel">
          <div className="segmented block card-lead">
            {(["login", "register"] as const).map((m) => (
              <button key={m} className={mode === m ? "active" : ""} onClick={() => setMode(m)}>
                {t(`account.${m}`)}
              </button>
            ))}
          </div>

          <div className="field">
            <label>{t("account.email")}</label>
            <input className="input" value={email} onChange={(e) => setEmail(e.target.value)} type="email" placeholder="you@example.com" autoComplete="email" />
          </div>
          <div className="field">
            <label>{t("account.password")}</label>
            <input className="input" value={password} onChange={(e) => setPassword(e.target.value)} type="password" placeholder="••••••••" onKeyDown={(e) => e.key === "Enter" && !disabled && submit()} />
          </div>
          <button className="btn block lg" onClick={submit} disabled={disabled}>
            {busy ? "…" : mode === "login" ? t("account.signIn") : t("account.createAccount")}
          </button>

          <div className="divider">{t("account.or")}</div>
          <div className="stack-gap">
            <button className="btn oauth" onClick={() => oauth("google")} disabled={disabled}><GoogleIcon size={17} />{t("account.continueWithGoogle")}</button>
            <button className="btn oauth" onClick={() => oauth("github")} disabled={disabled}><GitHubIcon size={17} />{t("account.continueWithGithub")}</button>
          </div>
          {oauthBusy && <p className="auth-note">{t("account.waitingAuth")}</p>}

          {!inTauri && <Err>{t("account.runInside")}</Err>}
          <Err>{err}</Err>
        </div>
      </div>
    </div>
  );
}
