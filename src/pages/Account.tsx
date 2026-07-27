import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { countryOptions, guessCountry } from "@shared/countries";
import { invoke, inTauri, runOAuthFlow, type Profile } from "../lib";
import { ProfileCenter } from "./account/ProfileCenter";
import { GitHubIcon, GoogleIcon } from "./account/icons";
import { Err } from "../ui";

export function Account() {
  const { t, i18n } = useTranslation();
  const [mode, setMode] = useState<"login" | "register">("login");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  // Asked for once, at sign-up: the platform infers nothing about location (no
  // geolocation, no IP lookup), so this is where the country comes from. The
  // starting value is the desktop locale's region, not the network's.
  const [region, setRegion] = useState(() => guessCountry());
  const countries = useMemo(() => countryOptions(i18n.language), [i18n.language]);
  const [profile, setProfile] = useState<Profile | null>(null);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const [oauthBusy, setOauthBusy] = useState<string | null>(null);
  // Address awaiting e-mail verification. Set after signing up, and after a
  // correct-password sign-in the server refused because the address was never
  // confirmed; while set, this panel shows the check-your-inbox state.
  const [pending, setPending] = useState("");
  const [notice, setNotice] = useState("");

  useEffect(() => {
    if (inTauri) invoke<Profile>("me_profile").then(setProfile).catch(() => {});
  }, []);

  async function loadProfile() {
    setProfile(await invoke<Profile>("me_profile"));
    // Ensure the account has an asale API key as soon as it signs in.
    invoke("ensure_api_key").catch(() => {});
  }

  type AuthResult = { verification_required?: boolean; verification_sent?: boolean };

  async function submit() {
    setErr(""); setNotice(""); setBusy(true);
    try {
      const r = await (mode === "login"
        ? invoke<AuthResult>("login", { email, password })
        : invoke<AuthResult>("register", { email, password, region }));
      // Registering never opens a session, and an unverified sign-in is refused:
      // either way there is no profile to load yet.
      if (r?.verification_required) {
        setPending(email);
        if (mode === "register" && r.verification_sent === false) {
          setErr(t("account.verifySendFailed"));
        }
        return;
      }
      await loadProfile();
    } catch (e) { setErr(String((e as Error).message)); } finally { setBusy(false); }
  }

  async function resendVerification() {
    setErr(""); setNotice(""); setBusy(true);
    try {
      await invoke("resend_verification", { email: pending });
      setNotice(t("account.verifyResent"));
    } catch (e) { setErr(String((e as Error).message)); } finally { setBusy(false); }
  }

  function leavePending() {
    setPending(""); setErr(""); setNotice(""); setPassword(""); setMode("login");
  }

  async function oauth(provider: "google" | "github") {
    setErr(""); setOauthBusy(provider);
    // The country rides along only while signing up; the server keeps it only
    // if this exchange creates the account.
    try {
      await runOAuthFlow("platform_oauth_login", {
        provider,
        link: false,
        region: mode === "register" ? region : "",
      });
      await loadProfile();
    }
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
          <h1>
            {pending
              ? t("account.verifySentTitle")
              : mode === "login"
                ? t("account.signIn")
                : t("account.createAccount")}
          </h1>
          <p className="sub">
            {pending ? t("account.verifySentSubtitle", { email: pending }) : t("account.subSignedOut")}
          </p>
        </div>

        {pending ? (
          <div className="card auth-panel">
            <p className="auth-note">{t("account.verifySentHint")}</p>
            <button className="btn block lg" onClick={resendVerification} disabled={busy}>
              {busy ? "…" : t("account.verifyResend")}
            </button>
            <button className="btn block" onClick={leavePending} disabled={busy}>
              {t("account.backToSignIn")}
            </button>
            {notice && <p className="auth-note">{notice}</p>}
            <Err>{err}</Err>
          </div>
        ) : (
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
          {mode === "register" && (
            <div className="field">
              <label>{t("account.region")}</label>
              <select className="input" value={region} onChange={(e) => setRegion(e.target.value)}>
                <option value="">{t("account.regionPlaceholder")}</option>
                {countries.map((c) => (
                  <option key={c.code} value={c.code}>{c.name}</option>
                ))}
              </select>
              <span className="hint">{t("account.regionHint")}</span>
            </div>
          )}
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
        )}
      </div>
    </div>
  );
}
