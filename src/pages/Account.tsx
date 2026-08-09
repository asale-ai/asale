import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { countryOptions, detectCountryByIp, guessCountry } from "@shared/countries";
import { CountrySelect } from "../components/CountrySelect";
import { invoke, inTauri, runOAuthFlow, takeSignInReason, type Profile } from "../lib";
import { ProfileCenter } from "./account/ProfileCenter";
import { GitHubIcon, GoogleIcon } from "./account/icons";
import { Err, Skeleton } from "../ui";
import { IconInfo } from "../icons";
import { errText } from "../errors";

export function Account() {
  const { t, i18n } = useTranslation();
  const [mode, setMode] = useState<"login" | "register">("login");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  // Asked for once, at sign-up: this is the only place the platform learns the
  // country. Pre-filled twice over — from the desktop locale immediately, then
  // from an IP lookup a moment later if that answers with something better.
  // Both are guesses at a field the user can change; see `shared/countries`.
  const [region, setRegion] = useState(() => guessCountry());
  /** Set the moment the user touches the picker, which ends the guessing. A ref
   *  because the only reader is a promise callback that would otherwise close
   *  over whatever this was at mount — always `false`. */
  const regionTouched = useRef(false);
  const countries = useMemo(() => countryOptions(i18n.language), [i18n.language]);
  // `undefined` until the profile call answers: rendering the sign-in form in
  // the meantime tells a signed-in user they are signed out.
  const [profile, setProfile] = useState<Profile | null | undefined>(undefined);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const [oauthBusy, setOauthBusy] = useState<string | null>(null);
  // Address awaiting e-mail verification. Set after signing up, and after a
  // correct-password sign-in the server refused because the address was never
  // confirmed; while set, this panel shows the check-your-inbox state.
  const [pending, setPending] = useState("");
  const [notice, setNotice] = useState("");
  // Set when another page sent the user here — "sign in before buying tokens",
  // "your session has expired". Read once, on arrival: landing on a login form
  // you did not ask for needs the sentence that explains it.
  const [sentHere] = useState(() => takeSignInReason());

  useEffect(() => {
    if (!inTauri) { setProfile(null); return; }
    invoke<Profile>("me_profile").then(setProfile).catch(() => setProfile(null));
  }, []);

  // The IP guess, which lands a beat after first paint. It only overwrites a
  // value the user has not touched — the locale guess is already in the field
  // by then, and silently swapping a country somebody just picked would be a
  // bug they would have to catch by re-reading the form.
  useEffect(() => {
    let live = true;
    detectCountryByIp().then((code) => {
      if (live && code && !regionTouched.current) setRegion(code);
    });
    return () => { live = false; };
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
    } catch (e) { setErr(errText(e)); } finally { setBusy(false); }
  }

  async function resendVerification() {
    setErr(""); setNotice(""); setBusy(true);
    try {
      // The call succeeding only means the server accepted the request; `sent`
      // is what says a mail left. An unknown or already-verified address also
      // reports `true` — the server will not say which addresses are
      // registered — so `false` specifically means the provider refused a link
      // this user is waiting for, and must not be dressed up as success.
      const r = await invoke<{ sent?: boolean }>("resend_verification", { email: pending });
      if (r?.sent === false) setErr(t("account.verifyResendFailed"));
      else setNotice(t("account.verifyResent"));
    } catch (e) { setErr(errText(e)); } finally { setBusy(false); }
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
    catch (e) { setErr(errText(e)); } finally { setOauthBusy(null); }
  }

  async function logout() {
    try { await invoke("logout"); } catch { /* clear local state regardless */ }
    setProfile(null);
  }

  if (profile === undefined) {
    return (
      <div className="auth-wrap">
        <div className="auth-card" aria-busy="true">
          <div className="auth-head">
            <div className="logo">
              <img className="logo-mark" src="/logo.svg" alt="Asale" />
            </div>
            <Skeleton w={148} h={22} r={9} style={{ margin: "0 auto 10px" }} />
            <Skeleton w={220} h={12} style={{ margin: "0 auto" }} />
          </div>
          <div className="card auth-panel">
            <Skeleton h={38} r={10} style={{ marginBottom: 10 }} />
            <Skeleton h={38} r={10} style={{ marginBottom: 16 }} />
            <Skeleton h={40} r={10} />
          </div>
        </div>
      </div>
    );
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
            <p className="auth-note auth-hint">{t("account.verifySentHint")}</p>
            <div className="stack-gap">
              <button className="btn block lg" onClick={leavePending} disabled={busy}>
                {t("account.backToSignIn")}
              </button>
              <button className="btn ghost block" onClick={resendVerification} disabled={busy}>
                {busy ? "…" : t("account.verifyResend")}
              </button>
            </div>
            {notice && <p className="auth-note">{notice}</p>}
            <Err>{err}</Err>
          </div>
        ) : (
        <div className="card auth-panel">
          {sentHere && (
            <div className="callout info compact card-lead">
              <IconInfo /><span>{t(sentHere)}</span>
            </div>
          )}
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
              <CountrySelect
                value={region}
                countries={countries}
                placeholder={t("account.regionPlaceholder")}
                onChange={(code) => { regionTouched.current = true; setRegion(code); }}
              />
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
