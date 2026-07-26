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
//              See .env.example. Gemini login is disabled when neither is set.
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

/// Provider OAuth endpoints/client (spec §3.2 constants).
#[derive(Clone)]
pub struct OAuthProvider {
    pub name: String,
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scope: &'static str,
    /// Claude adds `code=true` to the authorize query (matches the CLI flow).
    pub anthropic_style: bool,
}

pub fn provider(name: &str) -> Option<OAuthProvider> {
    Some(match name {
        "claude" | "claude_work" => OAuthProvider {
            name: name.to_string(),
            authorize_url: "https://claude.ai/oauth/authorize",
            token_url: "https://api.anthropic.com/v1/oauth/token",
            // Same Anthropic client for Code and Work (only the UA differs, §3.2).
            client_id: env_or("ASALE_OAUTH_CLIENT_ID_CLAUDE", CLAUDE_CLIENT_ID),
            client_secret: None,
            scope: "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
            anthropic_style: true,
        },
        "codex" => OAuthProvider {
            name: name.to_string(),
            authorize_url: "https://auth.openai.com/oauth/authorize",
            token_url: "https://auth.openai.com/oauth/token",
            client_id: env_or("ASALE_OAUTH_CLIENT_ID_CODEX", CODEX_CLIENT_ID),
            client_secret: None,
            scope: "openid profile email offline_access",
            anthropic_style: false,
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
            anthropic_style: false,
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

/// Result of an authorization: the captured code + the callback redirect uri.
pub struct AuthCode {
    pub code: String,
    pub redirect_uri: String,
    pub verifier: String,
}

/// Start a localhost callback listener and return the authorize URL to open.
/// The returned future resolves once the browser redirects back with a code.
pub async fn begin(p: &OAuthProvider) -> anyhow::Result<(String, AuthCodeFuture)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
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
    if p.anthropic_style {
        // The Claude CLI flow requests an authorization code explicitly.
        url.push_str("&code=true");
    }

    let (tx, rx) = oneshot::channel::<String>();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
    let redirect2 = redirect_uri.clone();
    let verifier = pkce.verifier.clone();

    // Minimal callback server: capture ?code= and close.
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let code = extract_query(&req, "code");
            let body = "<html><body style='font-family:sans-serif'>asale: authorization received. You can close this window.</body></html>";
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
            let _ = sock.write_all(resp.as_bytes()).await;
            if let Some(sender) = tx.lock().unwrap().take() {
                let _ = sender.send(code);
            }
        }
    });

    Ok((url, AuthCodeFuture { rx, redirect_uri: redirect2, verifier }))
}

/// Awaitable that yields the captured auth code.
pub struct AuthCodeFuture {
    rx: oneshot::Receiver<String>,
    redirect_uri: String,
    verifier: String,
}

impl AuthCodeFuture {
    pub async fn wait(self) -> anyhow::Result<AuthCode> {
        let code = self.rx.await.map_err(|_| anyhow::anyhow!("callback closed"))?;
        if code.is_empty() {
            anyhow::bail!("no code in callback");
        }
        Ok(AuthCode { code, redirect_uri: self.redirect_uri, verifier: self.verifier })
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
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let code = extract_query(&req, "code");
            let state = extract_query(&req, "state");
            let body = "<!doctype html><html><head><meta charset='utf-8'><title>asale</title></head>\
                <body style='font-family:sans-serif;display:flex;align-items:center;justify-content:center;height:90vh'>\
                <div style='text-align:center'><h2>登录成功，请返回应用</h2>\
                <p style='color:#666'>Signed in — you can return to the app and close this tab.</p></div></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
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
        // Anthropic's token endpoint takes a JSON body.
        let body: serde_json::Map<String, serde_json::Value> =
            params.iter().map(|(k, v)| (k.to_string(), serde_json::Value::String(v.clone()))).collect();
        http.post(p.token_url).json(&body).send().await?
    } else {
        http.post(p.token_url).form(&params).send().await?
    };
    let v: serde_json::Value = resp.json().await?;
    Ok(v)
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
