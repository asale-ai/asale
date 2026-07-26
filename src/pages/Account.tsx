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
        <div style={{ textAlign: "center", marginBottom: 22 }}>
          <div className="logo" style={{ justifyContent: "center", padding: 0, marginBottom: 14 }}>
            <img className="logo-mark" src="/logo.svg" alt="Asale" style={{ width: 40, height: 40 }} />
          </div>
          <h1 style={{ fontSize: 22 }}>{mode === "login" ? t("account.signIn") : t("account.createAccount")}</h1>
          <p className="muted" style={{ fontSize: 13.5, marginTop: 4 }}>{t("account.subSignedOut")}</p>
        </div>

        <div className="card" style={{ marginBottom: 0 }}>
          <div className="segmented" style={{ width: "100%", marginBottom: 18 }}>
            {(["login", "register"] as const).map((m) => (
              <button key={m} className={mode === m ? "active" : ""} style={{ flex: 1, justifyContent: "center" }} onClick={() => setMode(m)}>
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
          <button className="btn oauth" onClick={() => oauth("google")} disabled={disabled}><GoogleIcon size={18} />{t("account.continueWithGoogle")}</button>
          <button className="btn oauth" style={{ marginTop: 8 }} onClick={() => oauth("github")} disabled={disabled}><GitHubIcon size={18} />{t("account.continueWithGithub")}</button>
          {oauthBusy && <p className="muted" style={{ fontSize: 13, marginTop: 10, textAlign: "center" }}>{t("account.waitingAuth")}</p>}

          {!inTauri && <Err>{t("account.runInside")}</Err>}
          <Err>{err}</Err>
        </div>
      </div>
    </div>
  );
}
