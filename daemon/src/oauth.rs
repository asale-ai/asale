//! OAuth PKCE flow (spec §3.2). Opens the provider authorize URL in the browser,
//! captures the code on a localhost callback, and exchanges it for tokens. The
//! tokens are handed to the encrypted secret store by the caller — never persisted in plaintext.

use base64::Engine;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::oneshot;

const B64URL: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

// ── Public OAuth client credentials (spec §3.2) ─────────────────────────────
//
// These are the *public* client identifiers published in each vendor's official
// CLI, cross-checked against the reference implementations:
//   - Claude:  CLIProxyAPI internal/auth/claude/anthropic_auth.go  (client_id
//              `9d1c250a-…`, authorize claude.ai, token api.anthropic.com). There
//              is NO separate "Claude Work" client — the same id is reused and
//              only the UA profile differs (confirmed: no work/console/desktop id
//              exists in cc-switch or CLIProxyAPI).
//   - Codex:   CLIProxyAPI openai_auth.go / cc-switch codex_oauth_auth.rs
//              (`app_EMoam…`).
//   - Gemini:  cc-switch subscription.rs (gemini-cli installed-app id/secret,
//              public values from google-gemini/gemini-cli). Google installed-app
//              flows require a client *secret* at token exchange, so — even though
//              the value is published upstream — it is NOT checked into this repo
//              (GitHub push protection rejects it, and vendoring a credential in
//              git is a bad default anyway). Supply it instead via env:
//                - at build time: ASALE_OAUTH_CLIENT_ID_GEMINI /
//                  ASALE_OAUTH_CLIENT_SECRET_GEMINI are baked in by `option_env!`
//                - at run time:   the same two vars override whatever was baked in
//              See .env.package.example. Gemini login is disabled when neither is set.
//
// Any value may be overridden at runtime via env (compile-time defaults).

const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const GEMINI_CLIENT_ID: &str = match option_env!("ASALE_OAUTH_CLIENT_ID_GEMINI") {
    Some(v) => v,
    None => "",
};
pub const GEMINI_CLIENT_SECRET: &str = match option_env!("ASALE_OAUTH_CLIENT_SECRET_GEMINI") {
    Some(v) => v,
    None => "",
};

/// Resolve a client credential, honoring an env override.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
}

pub fn claude_client_id() -> String {
    env_or("ASALE_OAUTH_CLIENT_ID_CLAUDE", CLAUDE_CLIENT_ID)
}
pub fn codex_client_id() -> String {
    env_or("ASALE_OAUTH_CLIENT_ID_CODEX", CODEX_CLIENT_ID)
}
pub fn gemini_client_id() -> String {
    env_or("ASALE_OAUTH_CLIENT_ID_GEMINI", GEMINI_CLIENT_ID)
}
pub fn gemini_client_secret() -> String {
    env_or("ASALE_OAUTH_CLIENT_SECRET_GEMINI", GEMINI_CLIENT_SECRET)
}

/// A provider asale knows, whose client credentials this build was not given.
///
/// [`provider`] answers `None` for this and for a name it has never heard of,
/// which is fine for deciding whether a flow can start and wrong for saying why
/// it cannot: a client packaged without Gemini's Google id/secret reported
/// `unknown provider`, sending the reader to hunt for a typo in a name they had
/// picked off a button. Kept beside the guard in [`provider`] so the two cannot
/// drift apart.
pub fn provider_unconfigured(name: &str) -> bool {
    name == "gemini" && (gemini_client_id().is_empty() || gemini_client_secret().is_empty())
}

/// Provider OAuth endpoints/client (spec §3.2 constants).
#[derive(Clone)]
pub struct OAuthProvider {
    pub name: String,
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scope: &'static str,
    /// Extra authorize-query parameters the vendor's own CLI sends.
    pub extra_authorize_params: &'static [(&'static str, &'static str)],
    /// Anthropic's token endpoint takes a JSON body instead of a form post.
    pub anthropic_style: bool,
    /// Loopback port the provider's registered redirect pins us to. `None` =
    /// any ephemeral port is accepted (Anthropic, Google).
    pub redirect_port: Option<u16>,
    /// Path component of the registered redirect URI.
    pub redirect_path: &'static str,
}

pub fn provider(name: &str) -> Option<OAuthProvider> {
    Some(match name {
        // Every Claude family signs in through the same Anthropic client; what
        // separates them is the UA profile and, for `claude_extra`, what a buyer
        // may be matched to it.
        p if asale_protocol::ids::is_claude_family(p) => OAuthProvider {
            name: name.to_string(),
            authorize_url: "https://claude.ai/oauth/authorize",
            token_url: "https://api.anthropic.com/v1/oauth/token",
            // Same Anthropic client for Code and Work (only the UA differs, §3.2).
            client_id: env_or("ASALE_OAUTH_CLIENT_ID_CLAUDE", CLAUDE_CLIENT_ID),
            client_secret: None,
            scope: "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
            // The Claude CLI flow requests an authorization code explicitly.
            extra_authorize_params: &[("code", "true")],
            anthropic_style: true,
            // Anthropic accepts any ephemeral loopback port on `/callback`
            // (claude-code builds `http://localhost:{port}/callback`).
            redirect_port: None,
            redirect_path: "/callback",
        },
        "codex" => OAuthProvider {
            name: name.to_string(),
            authorize_url: "https://auth.openai.com/oauth/authorize",
            token_url: "https://auth.openai.com/oauth/token",
            client_id: env_or("ASALE_OAUTH_CLIENT_ID_CODEX", CODEX_CLIENT_ID),
            client_secret: None,
            scope: "openid profile email offline_access",
            // What Codex CLI sends alongside. `id_token_add_organizations`
            // is what puts the ChatGPT account/plan claims in the id_token —
            // without it the exchange returns a token we cannot attribute to
            // an account.
            extra_authorize_params: &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "codex_cli"),
            ],
            anthropic_style: false,
            // OpenAI registered exactly one redirect for this public client:
            // the port and path Codex CLI itself listens on. A loopback URI
            // that differs in either — a random port, or `/callback` — is
            // refused before the login page even renders, with
            // `authorize_hydra_invalid_request`. `localhost` is also part of
            // the match: `127.0.0.1` is a different string, hence a different
            // redirect_uri.
            redirect_port: Some(1455),
            redirect_path: "/auth/callback",
        },
        // Unconfigured Gemini credentials would only fail later at token exchange
        // with an opaque Google error — refuse the provider up front instead.
        "gemini" if gemini_client_id().is_empty() || gemini_client_secret().is_empty() => return None,
        "gemini" => OAuthProvider {
            name: name.to_string(),
            authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
            token_url: "https://oauth2.googleapis.com/token",
            client_id: gemini_client_id(),
            client_secret: Some(gemini_client_secret()),
            scope: "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email",
            extra_authorize_params: &[],
            anthropic_style: false,
            // Google installed-app clients ignore the loopback port but match
            // the path; gemini-cli registers `/oauth2callback`.
            redirect_port: None,
            redirect_path: "/oauth2callback",
        },
        _ => return None,
    })
}

/// PKCE pair.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn gen_pkce() -> Pkce {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    let verifier = B64URL.encode(buf);
    let challenge = B64URL.encode(Sha256::digest(verifier.as_bytes()));
    Pkce { verifier, challenge }
}

/// Hands an authorization code to a flow that is waiting on its loopback
/// callback, without the callback ever being reached.
///
/// The listener and this share one sender, so whichever arrives first wins and
/// the other finds the slot empty. That is what lets a browser on *another*
/// machine finish a login: its `localhost` is its own machine, so the redirect
/// lands nowhere and the user pastes the address back instead.
#[derive(Clone)]
pub struct CodeSubmitter(Arc<std::sync::Mutex<Option<oneshot::Sender<(String, String)>>>>);

impl CodeSubmitter {
    /// False when the flow is already over — the callback beat us to it, or a
    /// code was submitted before.
    pub fn submit(&self, code: String) -> bool {
        match self.0.lock().unwrap().take() {
            // No echoed state: what the user pastes back may be the bare code,
            // and [`AuthCodeFuture::wait`] treats an absent one as "this vendor
            // did not send it" rather than as a mismatch. Nothing is lost — the
            // paste is already bound to a flow the caller had to name, and the
            // exchange replays the state this flow started with either way.
            Some(tx) => tx.send((code, String::new())).is_ok(),
            None => false,
        }
    }
}

/// Pull the authorization code out of whatever the user pasted back.
///
/// Three shapes reach this, because all three are what a person actually has in
/// hand when the callback page will not open:
///
///   - the whole redirect URL from the address bar,
///     `http://localhost:37669/callback?code=…&state=…`
///   - just the query, with or without its `?`
///   - the bare code — which is what Anthropic's authorize page shows, in the
///     form `code#state`, when it cannot redirect
///
/// A URL carrying no code at all (`?error=access_denied`) is rejected rather
/// than passed through as if the whole string were a code: the exchange would
/// fail later with a vendor error nobody can act on.
pub fn extract_pasted_code(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    let query = s.rsplit_once('?').map(|(_, q)| q).unwrap_or(s);
    for pair in query.split('&') {
        if let Some(v) = pair.trim().strip_prefix("code=") {
            let code = urldecode(v.split('#').next().unwrap_or(v));
            let code = code.trim().to_string();
            return (!code.is_empty()).then_some(code);
        }
    }
    // Nothing named `code`. Only a bare token can still be one — anything with
    // a scheme or another `key=value` in it is a URL that simply lacks a code.
    if s.contains("://") || s.contains('=') {
        return None;
    }
    let bare = s.split('#').next().unwrap_or(s).trim().to_string();
    (!bare.is_empty()).then_some(bare)
}

/// Result of an authorization: the captured code + the callback redirect uri.
pub struct AuthCode {
    pub code: String,
    pub redirect_uri: String,
    pub verifier: String,
    /// The `state` this login was started with. Carried past the callback
    /// because Anthropic's token endpoint requires it in the exchange body too
    /// — see [`exchange`].
    pub state: String,
}

/// Start a localhost callback listener and return the authorize URL to open.
/// The returned future resolves once the browser redirects back with a code.
pub async fn begin(p: &OAuthProvider) -> anyhow::Result<(String, AuthCodeFuture)> {
    let want_port = p.redirect_port.unwrap_or(0);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", want_port)).await.map_err(|e| {
        match p.redirect_port {
            // A pinned port that is already taken is almost always the vendor's
            // own CLI mid-login — say so instead of surfacing EADDRINUSE.
            Some(port) => anyhow::anyhow!(
                "loopback port {port} is busy — close any running `{}` login and retry ({e})",
                p.name
            ),
            None => anyhow::anyhow!("cannot bind loopback callback: {e}"),
        }
    })?;
    let port = listener.local_addr()?.port();
    // Host must be `localhost`, not `127.0.0.1`: the vendors' public CLI clients
    // whitelist the literal hostname, and Anthropic rejects the dotted-quad form
    // outright ("Redirect URI http://127.0.0.1:… is not supported by client").
    let redirect_uri = format!("http://localhost:{port}{}", p.redirect_path);
    // `localhost` may resolve to ::1 first, so answer on both loopback stacks.
    let listener_v6 = tokio::net::TcpListener::bind(("::1", port)).await.ok();
    let pkce = gen_pkce();
    let state = uuid::Uuid::new_v4().to_string();

    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        p.authorize_url,
        urlencode(&p.client_id),
        urlencode(&redirect_uri),
        urlencode(p.scope),
        pkce.challenge,
        state,
    );
    for (k, v) in p.extra_authorize_params {
        url.push_str(&format!("&{}={}", urlencode(k), urlencode(v)));
    }

    // `(code, state)`: the state comes back so the callback can be checked
    // against the login that started it.
    let (tx, rx) = oneshot::channel::<(String, String)>();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
    let tx2 = tx.clone();
    let redirect2 = redirect_uri.clone();
    let verifier = pkce.verifier.clone();
    let state2 = state.clone();

    // Minimal callback server: capture ?code= and close. It keeps accepting
    // until a request actually carries a code — a browser will happily spend
    // the first connection on a favicon or a probe, and taking that one as
    // "the callback" would fail the login with an empty code.
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let accepted = match &listener_v6 {
                Some(v6) => tokio::select! {
                    r = listener.accept() => r,
                    r = v6.accept() => r,
                },
                None => listener.accept().await,
            };
            let Ok((mut sock, _)) = accepted else { break };
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let code = extract_query(&req, "code");
            // The provider can redirect back with `?error=access_denied` when the
            // user declines — that is not a success, so don't claim it is. A
            // request carrying neither is not the callback at all (favicon, probe):
            // answer it and keep waiting for the real one.
            let denied = !extract_query(&req, "error").is_empty();
            if code.is_empty() && !denied {
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
                continue;
            }
            let lang = pick_lang(&req);
            let body = if code.is_empty() {
                landing_page(&AUTHORIZE_FAIL, lang, false)
            } else {
                landing_page(&AUTHORIZE_OK, lang, true)
            };
            write_page(&mut sock, &body).await;
            if let Some(sender) = tx.lock().unwrap().take() {
                let _ = sender.send((code, extract_query(&req, "state")));
            }
            break;
        }
    });

    Ok((
        url,
        AuthCodeFuture { rx, redirect_uri: redirect2, verifier, state: state2, submitter: CodeSubmitter(tx2) },
    ))
}

/// Awaitable that yields the captured auth code.
pub struct AuthCodeFuture {
    rx: oneshot::Receiver<(String, String)>,
    redirect_uri: String,
    verifier: String,
    state: String,
    submitter: CodeSubmitter,
}

impl AuthCodeFuture {
    /// The other way to finish this flow: a code pasted by the user, for when
    /// the callback cannot reach this machine. Take it before the future is
    /// moved into the task that awaits it.
    pub fn submitter(&self) -> CodeSubmitter {
        self.submitter.clone()
    }

    pub async fn wait(self) -> anyhow::Result<AuthCode> {
        let (code, echoed_state) = self.rx.await.map_err(|_| anyhow::anyhow!("callback closed"))?;
        if code.is_empty() {
            anyhow::bail!("no code in callback");
        }
        // Checked only when the provider echoes one: every provider here is sent
        // a `state` and returns it, but a login must not start failing because
        // some vendor drops it. A value that comes back *different* is another
        // matter — that callback belongs to a different login attempt.
        if !echoed_state.is_empty() && echoed_state != self.state {
            anyhow::bail!("state mismatch");
        }
        Ok(AuthCode {
            code,
            redirect_uri: self.redirect_uri,
            verifier: self.verifier,
            state: self.state,
        })
    }
}

/// Loopback callback for the server-driven platform OAuth flow (Google/GitHub
/// login to the asale account). The server builds the authorize URL; we only
/// host the redirect_uri and capture `code` + `state`.
pub struct PlatformCallback {
    pub redirect_uri: String,
    rx: oneshot::Receiver<(String, String)>,
}

/// Bind a loopback listener on a random port and return the callback handle.
pub async fn begin_platform_loopback() -> anyhow::Result<PlatformCallback> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let (tx, rx) = oneshot::channel::<(String, String)>();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));

    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let code = extract_query(&req, "code");
            let state = extract_query(&req, "state");
            let lang = pick_lang(&req);
            let body = if code.is_empty() {
                landing_page(&LOGIN_FAIL, lang, false)
            } else {
                landing_page(&LOGIN_OK, lang, true)
            };
            write_page(&mut sock, &body).await;
            if let Some(sender) = tx.lock().unwrap().take() {
                let _ = sender.send((code, state));
            }
        }
    });

    Ok(PlatformCallback { redirect_uri, rx })
}

impl PlatformCallback {
    /// Wait for the browser redirect; validates `state` and enforces a timeout.
    pub async fn wait(self, expect_state: &str, timeout: std::time::Duration) -> anyhow::Result<String> {
        let (code, state) = tokio::time::timeout(timeout, self.rx)
            .await
            .map_err(|_| anyhow::anyhow!("authorization timed out"))?
            .map_err(|_| anyhow::anyhow!("callback closed"))?;
        if code.is_empty() {
            anyhow::bail!("no code in callback");
        }
        if state != expect_state {
            anyhow::bail!("state mismatch");
        }
        Ok(code)
    }
}

/// Exchange an auth code for tokens. Returns the raw token JSON.
pub async fn exchange(p: &OAuthProvider, ac: &AuthCode) -> anyhow::Result<serde_json::Value> {
    // Provider token endpoints are region-blocked in some markets; go through
    // the same proxy-aware client the refresh loop uses.
    let http = asale_client_core::http::upstream();
    let mut params = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", ac.code.clone()),
        ("redirect_uri", ac.redirect_uri.clone()),
        ("client_id", p.client_id.clone()),
        ("code_verifier", ac.verifier.clone()),
    ];
    // Google installed-app exchange requires the (public) client_secret.
    if let Some(secret) = &p.client_secret {
        params.push(("client_secret", secret.clone()));
    }
    let resp = if p.anthropic_style {
        // Anthropic's token endpoint takes a JSON body, and — unlike plain
        // OAuth2, where `state` belongs to the authorize step alone — it also
        // requires `state` here. Without it the whole request is refused as
        // `invalid_request_error: Invalid request format`, which reads as if
        // the login itself failed even though the browser already said it
        // succeeded.
        let mut body: serde_json::Map<String, serde_json::Value> =
            params.iter().map(|(k, v)| (k.to_string(), serde_json::Value::String(v.clone()))).collect();
        body.insert("state".to_string(), serde_json::Value::String(ac.state.clone()));
        http.post(p.token_url).json(&body).send().await?
    } else {
        http.post(p.token_url).form(&params).send().await?
    };
    let v: serde_json::Value = resp.json().await?;
    Ok(v)
}

/// Read a claim out of an OIDC `id_token`, unverified. The token came straight
/// off the provider's TLS token endpoint a moment ago and the value is only
/// used to label the account locally, so there is nothing here to forge.
/// Providers that return no `id_token` (Claude) simply yield `None`.
pub fn id_token_claim(tokens: &serde_json::Value, claim: &str) -> Option<String> {
    let payload = tokens["id_token"].as_str()?.split('.').nth(1)?;
    let json: serde_json::Value = serde_json::from_slice(&B64URL.decode(payload).ok()?).ok()?;
    Some(json.get(claim)?.as_str()?.to_string())
}

/// Callback-page copy in the four languages the app itself ships, in the order
/// `LANG_TAGS` indexes: en, zh, zh-TW, ja.
struct Copy {
    headline: [&'static str; 4],
    sub: [&'static str; 4],
}

const LANG_TAGS: [&str; 4] = ["en", "zh", "zh-TW", "ja"];

const AUTHORIZE_OK: Copy = Copy {
    headline: ["Authorized", "授权成功", "授權成功", "認証が完了しました"],
    sub: [
        "Account connected. You can close this tab and return to the app.",
        "账号已连接，可以关闭此页面并返回应用。",
        "帳號已連接，可以關閉此頁面並返回應用。",
        "アカウントを接続しました。このタブを閉じてアプリに戻ってください。",
    ],
};

const AUTHORIZE_FAIL: Copy = Copy {
    headline: ["Authorization incomplete", "授权未完成", "授權未完成", "認証が完了しませんでした"],
    sub: [
        "It was cancelled or has expired. Return to the app and try again.",
        "授权被取消或已过期，请回到应用重试。",
        "授權被取消或已過期，請回到應用重試。",
        "キャンセルされたか有効期限が切れました。アプリに戻ってやり直してください。",
    ],
};

const LOGIN_OK: Copy = Copy {
    headline: ["Signed in", "登录成功", "登入成功", "ログインしました"],
    sub: [
        "You can close this tab and return to the app.",
        "可以关闭此页面并返回应用。",
        "可以關閉此頁面並返回應用。",
        "このタブを閉じてアプリに戻ってください。",
    ],
};

const LOGIN_FAIL: Copy = Copy {
    headline: ["Sign-in incomplete", "登录未完成", "登入未完成", "ログインが完了しませんでした"],
    sub: AUTHORIZE_FAIL.sub,
};

/// Index into `LANG_TAGS` for the browser's `Accept-Language`, mirroring the
/// app's own `detect()`: zh (Traditional when the tag says so), ja, else
/// English — which is also where every language we don't ship lands.
fn pick_lang(req: &str) -> usize {
    let header = req
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim().eq_ignore_ascii_case("accept-language").then(|| value.to_ascii_lowercase())
        })
        .unwrap_or_default();
    // q-values are ordered highest-first in practice, so the first tag we
    // actually speak wins; the header's own weights are not worth parsing.
    for tag in header.split(',') {
        let tag = tag.split(';').next().unwrap_or("").trim();
        if tag.starts_with("zh") {
            return if ["tw", "hk", "mo", "hant"].iter().any(|s| tag.contains(s)) { 2 } else { 1 };
        }
        if tag.starts_with("ja") {
            return 3;
        }
        if tag.starts_with("en") {
            return 0;
        }
    }
    0
}

/// The page the browser lands on after a provider (or the platform) redirects
/// back. Both loopback listeners serve it, so the two flows look the same.
///
/// Deliberately self-contained — no network at all: the callback host is a
/// four-line TCP server, and the tab often loads while the machine is mid-login,
/// so any external font/asset would just render as a broken box.
///
/// The inline script rewrites the address bar to drop `?code=…`: the raw
/// authorization code would otherwise sit in the URL bar, in history, and in
/// whatever syncs that history.
fn landing_page(copy: &Copy, lang: usize, tone_ok: bool) -> String {
    let (headline, sub, lang) = (copy.headline[lang], copy.sub[lang], LANG_TAGS[lang]);
    let (mark, mark_fg, mark_bg) = if tone_ok {
        // A check, drawn rather than an emoji so it can't fall back to a glyph
        // the platform renders in its own colour.
        ("M4 10.5l4 4 8-9", "var(--ok)", "var(--ok-soft)")
    } else {
        ("M10 5v7M10 15.2v.1", "var(--bad)", "var(--bad-soft)")
    };
    format!(
        r#"<!doctype html><html lang="{lang}"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>asale</title><style>
:root {{
  --bg:#f9f9fb; --card:#fff; --fg:#16171c; --muted:#626671; --border:#ebebf0;
  --ok:#14a06a; --ok-soft:rgba(20,160,106,.1); --bad:#d94a50; --bad-soft:rgba(217,74,80,.09);
  --shadow:0 20px 48px -24px rgba(18,20,28,.25);
  color-scheme:light;
}}
@media (prefers-color-scheme:dark) {{
  :root {{
    --bg:#0c0d11; --card:#15171c; --fg:#e9eaee; --muted:#9ca1ad; --border:#23262e;
    --ok:#3ecf8e; --ok-soft:rgba(62,207,142,.13); --bad:#f0666b; --bad-soft:rgba(240,102,107,.13);
    --shadow:0 20px 48px -22px rgba(0,0,0,.8);
    color-scheme:dark;
  }}
}}
* {{ box-sizing:border-box; }}
body {{
  margin:0; min-height:100vh; display:flex; align-items:center; justify-content:center;
  padding:24px; background:var(--bg); color:var(--fg);
  font:400 14px/1.6 -apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif;
  -webkit-font-smoothing:antialiased;
}}
.card {{
  width:100%; max-width:400px; padding:40px 32px 32px; text-align:center;
  background:var(--card); border:1px solid var(--border); border-radius:16px; box-shadow:var(--shadow);
}}
.badge {{
  width:52px; height:52px; margin:0 auto 20px; border-radius:50%;
  background:{mark_bg}; display:flex; align-items:center; justify-content:center;
}}
h1 {{ margin:0 0 8px; font-size:17px; font-weight:600; letter-spacing:-.01em; }}
p {{ margin:0; color:var(--muted); font-size:13px; }}
.brand {{
  margin-top:28px; padding-top:18px; border-top:1px solid var(--border);
  color:var(--muted); font-size:11px; letter-spacing:.14em; text-transform:uppercase;
}}
</style></head><body>
<div class="card">
  <div class="badge"><svg width="20" height="20" viewBox="0 0 20 20" fill="none"
    stroke="{mark_fg}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"
    aria-hidden="true"><path d="{mark}"/></svg></div>
  <h1>{headline}</h1>
  <p>{sub}</p>
  <div class="brand">asale</div>
</div>
<script>
  // Keep the one-time authorization code out of the URL bar and history.
  try {{ history.replaceState(null, "", location.pathname); }} catch (e) {{}}
</script>
</body></html>"#
    )
}

/// Serve one HTML response and let the socket close.
async fn write_page(sock: &mut tokio::net::TcpStream, body: &str) {
    use tokio::io::AsyncWriteExt;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    let _ = sock.flush().await;
}

fn extract_query(req: &str, key: &str) -> String {
    // First line: GET /callback?code=...&state=... HTTP/1.1
    let first = req.lines().next().unwrap_or("");
    if let Some(qpos) = first.find('?') {
        let rest = &first[qpos + 1..];
        let end = rest.find(' ').unwrap_or(rest.len());
        for pair in rest[..end].split('&') {
            let mut it = pair.splitn(2, '=');
            if it.next() == Some(key) {
                return urldecode(it.next().unwrap_or(""));
            }
        }
    }
    String::new()
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two halves of "why can this flow not start" must not drift: whenever
    /// `provider_unconfigured` claims a name is merely missing credentials,
    /// `provider` has to be the one refusing it, and a name that is not a
    /// provider at all must never be reported that way — answering a typo with
    /// instructions about packaging OAuth secrets helps nobody.
    ///
    /// Deliberately environment-agnostic: whether *this* build carries Gemini's
    /// Google id/secret is a packaging choice, and a test that demanded one
    /// answer would fail on half the machines that run it.
    #[test]
    fn unconfigured_only_ever_describes_a_provider_we_know() {
        for name in ["nope", "", "claude", "claude_work", "codex"] {
            assert!(!provider_unconfigured(name), "{name} has no credential gate");
        }
        assert_eq!(
            provider_unconfigured("gemini"),
            provider("gemini").is_none(),
            "gemini is refused if and only if its credentials are missing"
        );
    }

    /// Anthropic (and OpenAI) whitelist the loopback redirect by hostname, and
    /// reject `http://127.0.0.1:{port}/…` with "Redirect URI … is not supported
    /// by client" — the authorize URL must use `localhost`.
    #[tokio::test]
    async fn claude_authorize_url_uses_localhost_loopback() {
        let p = provider("claude_work").unwrap();
        let (url, fut) = begin(&p).await.unwrap();
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A"), "bad redirect in {url}");
        assert!(!url.contains("127.0.0.1"), "dotted-quad loopback in {url}");
        assert!(url.ends_with("&code=true"));
        // The exchange must replay the exact same value.
        assert!(fut.redirect_uri.starts_with("http://localhost:"));
        assert!(fut.redirect_uri.ends_with("/callback"));
    }

    /// The `state` put in the authorize URL is the one the exchange replays:
    /// Anthropic refuses a token request that omits it.
    #[tokio::test]
    async fn the_authorize_state_is_kept_for_the_exchange() {
        let p = provider("claude").unwrap();
        let (url, fut) = begin(&p).await.unwrap();
        assert!(url.contains(&format!("&state={}", fut.state)), "authorize state not in {url}");
        assert!(!fut.state.is_empty());
    }

    /// A callback echoing someone else's state belongs to another login; one
    /// echoing nothing is a vendor quirk and still has to work.
    #[tokio::test]
    async fn a_foreign_state_is_refused_and_a_missing_one_is_tolerated() {
        let pending = |state: &str| {
            let (tx, rx) = oneshot::channel::<(String, String)>();
            let fut = AuthCodeFuture {
                rx,
                redirect_uri: "http://localhost:1/callback".into(),
                verifier: "verifier".into(),
                state: state.into(),
                // Unused here: these cases drive the callback half directly.
                submitter: CodeSubmitter(Arc::new(std::sync::Mutex::new(None))),
            };
            (tx, fut)
        };

        let (tx, fut) = pending("ours");
        tx.send(("the-code".into(), "someone-elses".into())).unwrap();
        assert!(fut.wait().await.is_err(), "a mismatched state must not produce a code");

        let (tx, fut) = pending("ours");
        tx.send(("the-code".into(), String::new())).unwrap();
        let ac = fut.wait().await.unwrap();
        assert_eq!(ac.code, "the-code");
        assert_eq!(ac.state, "ours", "and the exchange still replays ours");

        let (tx, fut) = pending("ours");
        tx.send(("the-code".into(), "ours".into())).unwrap();
        assert_eq!(fut.wait().await.unwrap().state, "ours");
    }

    #[test]
    fn landing_page_renders_both_tones() {
        let ok = landing_page(&AUTHORIZE_OK, 1, true);
        assert!(ok.contains("授权成功") && ok.contains("var(--ok)"));
        // `format!` escaping mistakes show up as leftover doubled braces in CSS.
        assert!(!ok.contains("{{") && !ok.contains("}}"), "unescaped braces leaked into the page");
        // The code must never survive in the address bar.
        assert!(ok.contains("history.replaceState"));
        let bad = landing_page(&AUTHORIZE_FAIL, 1, false);
        assert!(bad.contains("var(--bad)") && !bad.contains("var(--ok)"));
        // Every language renders, and `<html lang>` follows the copy shown.
        for (i, tag) in LANG_TAGS.iter().enumerate() {
            let page = landing_page(&LOGIN_OK, i, true);
            assert!(page.contains(&format!("<html lang=\"{tag}\"")), "{tag} lang attr");
            assert!(page.contains(LOGIN_OK.headline[i]), "{tag} headline");
        }
    }

    #[test]
    fn accept_language_picks_the_shipped_locale() {
        let req = |al: &str| format!("GET /callback HTTP/1.1\r\nHost: x\r\nAccept-Language: {al}\r\n\r\n");
        assert_eq!(pick_lang(&req("zh-CN,zh;q=0.9,en;q=0.8")), 1);
        assert_eq!(pick_lang(&req("zh-TW,zh-Hant;q=0.9")), 2);
        assert_eq!(pick_lang(&req("zh-HK")), 2);
        assert_eq!(pick_lang(&req("ja-JP,ja;q=0.9")), 3);
        assert_eq!(pick_lang(&req("en-GB,en;q=0.9")), 0);
        // Anything we don't ship — and a request with no header at all — is English.
        assert_eq!(pick_lang(&req("ko-KR,ko;q=0.9")), 0);
        assert_eq!(pick_lang("GET /callback HTTP/1.1\r\n\r\n"), 0);
        // A language we do ship, listed after one we don't, still wins.
        assert_eq!(pick_lang(&req("ko-KR,ja;q=0.8")), 3);
    }

    #[test]
    fn codex_redirect_is_pinned_to_the_registered_uri() {
        let p = provider("codex").unwrap();
        assert_eq!(p.redirect_port, Some(1455));
        assert_eq!(p.redirect_path, "/auth/callback");
    }

    /// The whole redirect URL, straight out of the address bar of a browser
    /// that could not load it — the shape users actually have.
    #[test]
    fn pasted_full_url() {
        let url = "http://localhost:37669/callback?code=847W0rntCZdxsyYiZQhFLBHpGuC9x0dIh9vFRZz6786mGUrD&state=5275809b-5f70-4d18-9e05-d9be9901a952";
        assert_eq!(
            extract_pasted_code(url).as_deref(),
            Some("847W0rntCZdxsyYiZQhFLBHpGuC9x0dIh9vFRZz6786mGUrD")
        );
    }

    #[test]
    fn pasted_query_or_bare_code() {
        assert_eq!(extract_pasted_code("?code=abc&state=xyz").as_deref(), Some("abc"));
        assert_eq!(extract_pasted_code("code=abc&state=xyz").as_deref(), Some("abc"));
        assert_eq!(extract_pasted_code("  abc  ").as_deref(), Some("abc"));
        // Anthropic's authorize page shows `code#state` when it cannot redirect.
        assert_eq!(extract_pasted_code("abc#state-uuid").as_deref(), Some("abc"));
        assert_eq!(extract_pasted_code("?code=abc#state-uuid").as_deref(), Some("abc"));
    }

    #[test]
    fn pasted_code_is_percent_decoded() {
        // Same decoder the loopback callback uses, so both routes yield the
        // identical code — a mismatch here would fail only at exchange time.
        assert_eq!(extract_pasted_code("?code=a%2Fb%2Bc").as_deref(), Some("a/b+c"));
    }

    /// A redirect that carries no code is not a code. Passing the raw string
    /// through would trade a clear "nothing here" for a vendor error at
    /// exchange time that says nothing about what to do.
    #[test]
    fn pasted_without_a_code_is_rejected() {
        assert_eq!(extract_pasted_code("http://localhost:37669/callback?error=access_denied"), None);
        assert_eq!(extract_pasted_code("?state=xyz"), None);
        assert_eq!(extract_pasted_code("   "), None);
        assert_eq!(extract_pasted_code(""), None);
    }

    /// The two routes into a flow share one sender, so the first to arrive
    /// wins and the second is a no-op rather than a second login.
    #[tokio::test]
    async fn submitting_a_code_finishes_the_flow_once() {
        let p = provider("claude").unwrap();
        let (_url, fut) = begin(&p).await.unwrap();
        let submitter = fut.submitter();
        assert!(submitter.submit("pasted-code".into()));
        assert!(!submitter.submit("second-try".into()), "a finished flow accepts nothing more");
        assert_eq!(fut.wait().await.unwrap().code, "pasted-code");
    }
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}
