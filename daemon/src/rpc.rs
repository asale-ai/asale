//! HTTP surface of the daemon:
//!
//!   POST /rpc/{cmd}   — JSON-RPC-ish command dispatch onto `commands::*`.
//!                       Body = the command's args object (camelCase keys, the
//!                       same shape the old Tauri `invoke` used). 200 → result
//!                       JSON; 4xx/5xx → `{"error": "..."}`.
//!   POST /ui-session  — trade the daemon token for the browser's session
//!                       cookie. What the unlock page below posts.
//!   GET  /healthz     — liveness (also used by the shell to detect a running
//!                       daemon and by the frontend to detect the daemon).
//!   GET  /*           — the embedded web UI (../dist), so one `asaled` binary
//!                       serves the whole app for B/S access. Gated.
//!
//! Auth model:
//!   - **Every** /rpc request must present the token matching
//!     `~/.asale/daemon.token` (0600, generated on first run) — loopback
//!     included — either as `X-Asale-Token: <token>` or as the session cookie
//!     below. See `authorized` for what that does and does not protect.
//!   - **The UI itself is gated too.** A browser gets the app only once it has
//!     proved it holds the token; until then every path answers with the
//!     unlock page and nothing of the application is served. This is what the
//!     `?token=`/`#token=` URL is for — not a convenience that a stripped URL
//!     can skip. The proof is kept in an HttpOnly `asale_session` cookie
//!     (SameSite=Strict, expiring), so the credential itself never sits in the
//!     address bar, the history, or a script-readable place.
//!   - The Tauri shell is the one client that bypasses this: it serves the
//!     frontend from its own origin and seeds the webview's localStorage with
//!     the token in an initialization script, so it only ever talks to /rpc.
//!   - `/healthz` stays open: it is a liveness probe with no data in it, and
//!     the CLI uses it to tell "port is dead" from "port is guarded".
//!   - CSRF: /rpc only accepts `Content-Type: application/json` (axum's `Json`
//!     rejects others), so cross-origin browser calls always trigger a CORS
//!     preflight, and the CORS layer only admits known origins. The session
//!     cookie is `SameSite=Strict`, so it is never attached to a request another
//!     site originated.

use crate::commands;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};
use crate::cmd_err;

#[derive(Clone)]
pub struct Ctx {
    pub state: Arc<AppState>,
    /// Shared secret required on every request.
    pub token: Arc<String>,
}

/// Embedded production web UI (built by `pnpm build` into ../dist).
#[derive(rust_embed::RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../dist"]
struct Ui;

pub fn router(ctx: Ctx) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| {
            origin.to_str().is_ok_and(allowed_origin)
        }))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE, header::HeaderName::from_static("x-asale-token")]);

    Router::new()
        .route("/rpc/:cmd", post(rpc))
        .route("/ui-session", post(ui_session))
        .route("/healthz", get(healthz))
        .fallback(get(ui))
        // RPC arguments are small intent objects. State the boundary explicitly
        // instead of inheriting axum's larger default.
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .layer(cors)
        .with_state(ctx)
}

/// Origins allowed to call the RPC API cross-origin.
///
/// Two callers, and neither is same-origin:
///
///   * **the Tauri shell's webview**, whose origin is the platform's business,
///     not ours. macOS and Linux serve the app from the custom scheme
///     (`tauri://localhost`); Windows cannot — WebView2 has no custom-scheme
///     support — so there the very same app is served over
///     `http://tauri.localhost`. Leaving that one out made every packaged
///     Windows client fail every RPC and render "the local service is not
///     running", against a daemon that was answering perfectly well. It never
///     showed up in development because `pnpm dev:app` loads the frontend from
///     Vite, whose origin is `http://localhost:9173` and matches below.
///   * **a browser on this machine** pointed at the Vite dev server.
///
/// B/S access — the daemon serving the UI itself — is same-origin and never
/// reaches this at all.
fn allowed_origin(o: &str) -> bool {
    o == "tauri://localhost"
        || o == "https://tauri.localhost"
        || o == "http://tauri.localhost"
        || o.starts_with("http://localhost:")
        || o.starts_with("http://127.0.0.1:")
}

async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true, "name": "asaled", "version": env!("CARGO_PKG_VERSION") }))
}

/// Name of the cookie holding a browser's proof that it was given the token.
const SESSION_COOKIE: &str = "asale_session";

/// How long a browser stays unlocked before it has to present the token again.
///
/// `daemon.token` never rotates, so without an expiry the very first tokenized
/// URL would unlock that browser permanently — which is the shape of the hole
/// this gate exists to close. A day is long enough that a working session is
/// never interrupted and short enough that a borrowed or forgotten browser stops
/// being an open door.
const SESSION_MAX_AGE_SECS: u32 = 24 * 60 * 60;

/// The UI gate: a browser is served the application only after it has proved it
/// holds the daemon token.
///
/// The static bundle used to be open on the grounds that "it has to load before
/// it can present a token". That is true of the *first* request and nothing
/// else, and it left `http://host:9700/` — the URL you get by deleting the
/// token from the one that was handed to you — serving the whole app to anyone
/// who could reach the port. The RPC layer still refused them, so no data
/// leaked, but the app is not the thing to hand a stranger either: it names the
/// user's accounts and features, and it is a map of the API behind it.
///
/// So the first request is answered by `unlock_page` — a few hundred bytes that
/// are not the app — and the application is served only from the second request
/// on, once the token has been traded for a session cookie.
async fn ui(State(ctx): State<Ctx>, uri: Uri, headers: axum::http::HeaderMap) -> Response {
    // `?token=` on the URL: the form `asale url --host`, old bookmarks and any
    // link that predates the fragment spelling still carry. Trade it for the
    // cookie and bounce to a URL that no longer has it, so the credential does
    // not stay in the address bar or the session history.
    if let Some(t) = query_token(&uri) {
        return if secret_matches(&ctx.token, &t) {
            unlocked_redirect(uri.path(), &t)
        } else {
            unlock_page(true)
        };
    }
    if !cookie_matches(&ctx.token, &headers) {
        return unlock_page(false);
    }
    serve_asset(uri.path())
}

/// `POST /ui-session {"token": "..."}` — what the unlock page calls to turn a
/// token into this browser's session cookie.
///
/// A POST rather than another tokenized GET so the token is in a body: request
/// paths reach access logs, `Referer` headers and shell history, and bodies do
/// not.
async fn ui_session(State(ctx): State<Ctx>, Json(a): Json<Value>) -> Response {
    let presented = a["token"].as_str().unwrap_or("").trim();
    if !secret_matches(&ctx.token, presented) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(cmd_err!("errors.daemon.unauthorized", "that is not this daemon's token").to_json()),
        )
            .into_response();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::SET_COOKIE, session_cookie(presented))
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"ok":true}"#))
        .unwrap()
}

/// The `Set-Cookie` value that unlocks this browser.
///
/// `HttpOnly` keeps the token out of reach of any script on the page, so an
/// injected one cannot read it back out and replay it elsewhere. `SameSite=Strict`
/// means no request another site originated ever carries it, which is what lets
/// /rpc accept the cookie without opening a CSRF hole. No `Secure`: the daemon
/// speaks plain HTTP on loopback, and a `Secure` cookie there is simply dropped.
fn session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_MAX_AGE_SECS}"
    )
}

/// The `token` query parameter, if the URL carries one.
///
/// The token is URL-safe base64 (`[A-Za-z0-9_-]`), so there is nothing here to
/// percent-decode; a value that arrived escaped simply will not match, which is
/// the correct answer for a token that is not this daemon's.
fn query_token(uri: &Uri) -> Option<String> {
    uri.query()?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "token").then(|| v.to_string())
    })
}

/// Hand over the cookie and bounce to the same path without the query.
///
/// The token is put back on as a fragment, which is never sent to a server: it
/// is how the frontend fills the `localStorage` copy it uses for the
/// `X-Asale-Token` header and for the shareable link on the Settings page, and
/// the frontend scrubs it from the address bar as soon as it has read it.
fn unlocked_redirect(path: &str, token: &str) -> Response {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, format!("{path}#token={token}"))
        .header(header::SET_COOKIE, session_cookie(token))
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::REFERRER_POLICY, "no-referrer")
        .body(Body::empty())
        .unwrap()
}

/// What a browser without a session gets, in place of the application.
///
/// It is self-contained on purpose — it must not pull anything out of the
/// gated bundle. Its whole job is to find a token and post it: from the
/// `#token=` fragment when the user followed the URL `asaled` printed (the
/// fragment never reached the gate above, so this is the only code that can see
/// it), and otherwise from the person reading the page.
fn unlock_page(bad_token: bool) -> Response {
    let nonce = nonce();
    let msg = if bad_token {
        "That token does not match this daemon."
    } else {
        "This client is locked."
    };
    let html = format!(
        r##"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Asale — locked</title>
<style nonce="{nonce}">
 :root {{ color-scheme: light dark }}
 body {{ margin:0; min-height:100vh; display:grid; place-items:center;
        font:15px/1.6 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif;
        background:#0b0d10; color:#e6e8eb }}
 main {{ width:min(420px,90vw); padding:28px 30px; border-radius:14px;
        background:#14181d; border:1px solid #232a31 }}
 h1 {{ margin:0 0 6px; font-size:17px; font-weight:600 }}
 p {{ margin:0 0 18px; color:#98a2ad; font-size:13px }}
 form {{ display:flex; gap:8px }}
 input {{ flex:1; min-width:0; padding:9px 11px; border-radius:8px; font:inherit;
         background:#0b0d10; color:inherit; border:1px solid #2b333b }}
 button {{ padding:9px 16px; border:0; border-radius:8px; font:inherit;
          font-weight:600; background:#4c8dff; color:#fff; cursor:pointer }}
 .err {{ margin:14px 0 0; color:#ff8a8a; font-size:13px; min-height:1.6em }}
 code {{ color:#c8d0d8 }}
</style></head><body><main>
<h1>{msg}</h1>
<p>Open the URL <code>asale open</code> prints, or paste the token from
   <code>~/.asale/daemon.token</code>.</p>
<form id="f"><input id="t" type="password" autocomplete="off" spellcheck="false"
  placeholder="daemon token" aria-label="daemon token"><button>Unlock</button></form>
<p class="err" id="e" role="alert"></p>
</main><script nonce="{nonce}">
(function () {{
  var err = document.getElementById('e');
  function unlock(token, silent) {{
    return fetch('/ui-session', {{
      method: 'POST', credentials: 'same-origin',
      headers: {{ 'content-type': 'application/json' }},
      body: JSON.stringify({{ token: token }})
    }}).then(function (r) {{
      // Reload keeps the fragment, so the app still picks the token up for the
      // header it sends cross-origin.
      if (r.ok) {{ location.reload(); return true; }}
      if (!silent) err.textContent = 'That token does not match this daemon.';
      return false;
    }}).catch(function () {{
      if (!silent) err.textContent = 'Could not reach the local service.';
      return false;
    }});
  }}
  var hash = new URLSearchParams(location.hash.replace(/^#/, '')).get('token');
  if (hash) unlock(hash, true);
  document.getElementById('f').addEventListener('submit', function (ev) {{
    ev.preventDefault();
    err.textContent = '';
    var v = document.getElementById('t').value.trim();
    if (v) unlock(v, false);
  }});
}})();
</script></body></html>"##
    );
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header("x-frame-options", "DENY")
        .header(
            header::CONTENT_SECURITY_POLICY,
            format!(
                "default-src 'none'; style-src 'nonce-{nonce}'; script-src 'nonce-{nonce}'; \
                 connect-src 'self'; form-action 'none'; base-uri 'none'; frame-ancestors 'none'"
            ),
        )
        .body(Body::from(html))
        .unwrap()
}

/// A fresh CSP nonce, so the unlock page can carry its own inline style and
/// script without `'unsafe-inline'` — which would otherwise apply to every
/// injected script too.
fn nonce() -> String {
    use rand::RngCore;
    let mut raw = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw)
}

/// Serve the embedded UI; unknown paths fall back to index.html (the app is a
/// single page). An empty dist (API-only build) returns a plain hint instead.
fn serve_asset(path: &str) -> Response {
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let asset = Ui::get(path).or_else(|| Ui::get("index.html"));
    match asset {
        Some(f) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
                .header(header::REFERRER_POLICY, "no-referrer")
                .header("x-frame-options", "DENY")
                .header(
                    header::CONTENT_SECURITY_POLICY,
                    // frame-src: 「对话」页把 Studio 嵌进 iframe（src/pages/Studio.tsx）。
                    // 没有这一条时 default-src 'self' 兜底把它拦掉，页面一片空白。
                    "default-src 'self'; img-src 'self' data: https:; style-src 'self' 'unsafe-inline'; \
                     script-src 'self'; connect-src 'self' http://127.0.0.1:* http://localhost:* \
                     ws://127.0.0.1:* ws://localhost:*; frame-src https://studio.asale.ai; \
                     frame-ancestors 'none'; base-uri 'none'",
                )
                .body(Body::from(f.data.into_owned()))
                .unwrap()
        }
        None => (
            StatusCode::OK,
            "asaled is running (no web UI embedded — build the frontend with `pnpm build`, then rebuild asaled, or open the Vite dev server).",
        )
            .into_response(),
    }
}

/// Every request needs the shared token — including loopback.
///
/// Loopback used to be trusted unconditionally, which made "can open a TCP
/// connection to 127.0.0.1:9700" the whole authorization check: any other user
/// on the machine, any sandboxed or containerized process, anything that could
/// reach the port could read the wallet, flip the sell switches or start a
/// withdrawal.
///
/// This does *not* stop malware running as the same user — `daemon.token` is
/// mode 0600 in that user's home, so a process with that user's privileges can
/// simply read it. Nothing a local daemon does can defend against that. What it
/// does close is the gap between "reachable on the port" and "allowed to hold
/// the user's credentials", which are very different sets.
fn authorized(ctx: &Ctx, _peer: &SocketAddr, headers: &axum::http::HeaderMap) -> bool {
    token_matches(&ctx.token, headers) || cookie_matches(&ctx.token, headers)
}

/// The header spelling: what every non-browser caller uses, and what the
/// frontend sends when its origin is not the daemon's (the Tauri shell, and a
/// browser pointed at the Vite dev server).
fn token_matches(expected: &str, headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-asale-token")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| secret_matches(expected, t))
}

/// The cookie spelling: what a browser served by this daemon sends, having
/// traded the token for it at the gate. Safe to honour on /rpc because the
/// cookie is `SameSite=Strict` — no request another site started carries it.
fn cookie_matches(expected: &str, headers: &axum::http::HeaderMap) -> bool {
    cookie(headers, SESSION_COOKIE).is_some_and(|t| secret_matches(expected, t))
}

/// One cookie out of the `Cookie` header, by name.
fn cookie<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(header::COOKIE)?.to_str().ok()?.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == name).then_some(v.trim())
    })
}

/// The whole authorization decision, as a pure function over the expected token
/// and the presented one. An empty expected token authorizes nothing.
fn secret_matches(expected: &str, presented: &str) -> bool {
    !expected.is_empty() && constant_time_eq(presented.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ── argument decoding ───────────────────────────────────────────────────────
//
// The frontend sends camelCase; the Rust probes and older callers send
// snake_case. Both used to be spelled out by hand at every call site — 
// `need_str(a, "account_id", "accountId")`, twice per field, plus a tuple
// `match` per command to turn the pieces back into a `Result`. Serde does the
// whole job with `rename_all = "camelCase"` plus a snake_case `alias` on each
// multi-word field, and the arms below shrink to one line each.

/// Decode a command's arguments, reporting a failure the frontend can show.
fn args<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, commands::CmdError> {
    serde_json::from_value(v.clone()).map_err(|e| {
        cmd_err!("errors.daemon.badArguments", format!("bad arguments: {e}"), detail = e.to_string())
    })
}

/// Tell "field absent" from "field present and null" — see `KeyUpdateArgs`.
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(d).map(Some)
}

macro_rules! rpc_args {
    ($($name:ident { $($field:tt)* })*) => {
        $(
            #[derive(serde::Deserialize)]
            #[cfg_attr(test, derive(Debug))]
            #[serde(rename_all = "camelCase")]
            struct $name { $($field)* }
        )*
    };
}

rpc_args! {
    ProxyArgs      { mode: String, #[serde(default)] url: Option<String> }
    // Which self-check finding to repair, by its stable `Finding::id`.
    SelfCheckFixArgs { id: String }
    CredentialArgs { email: String, password: String }
    // Registration takes the country the sign-up screen collected; the server
    // requires it, and it is the only moment the platform can learn one.
    RegisterArgs   { email: String, password: String, #[serde(default)] region: String }
    EmailArgs      { email: String }
    ProviderArgs   { provider: String }
    OauthLoginArgs { provider: String, #[serde(default, alias = "open_local")] open_local: Option<bool> }
    OauthResultArgs{ #[serde(alias = "flow_id")] flow_id: String }
    // `input` is whatever the user had in hand: the redirect URL, its query, or
    // the bare code. Aliased to `code` so a caller that already split it out —
    // a script, a future device flow — does not have to know the difference.
    OauthCodeArgs { #[serde(alias = "flow_id")] flow_id: String, #[serde(alias = "code")] input: String }
    PlatformOauthArgs {
        provider: String,
        #[serde(default)] link: Option<bool>,
        #[serde(default, alias = "open_local")] open_local: Option<bool>,
        /// Only honoured when the exchange creates a new account.
        #[serde(default)] region: Option<String>,
    }
    ProfileArgs    {
        #[serde(default)] name: Option<String>,
        #[serde(default)] region: Option<String>,
        #[serde(default, alias = "avatar_url")] avatar_url: Option<String>,
    }
    PasswordArgs   {
        #[serde(alias = "new_password")] new_password: String,
        #[serde(default, alias = "old_password")] old_password: Option<String>,
    }
    GetSettingArgs { key: String }
    SetSettingArgs { key: String, value: String }
    AccountArgs    { provider: String, #[serde(alias = "account_id")] account_id: String }
    // The consumer API key list (`commands::apikeys`). `applyToTools` is the
    // answer to the prompt the UI shows before it moves the default: rewrite
    // the configs of the CLIs that are buying right now, or leave them holding
    // the key they have.
    KeyCreateArgs  {
        #[serde(default)] label: Option<String>,
        #[serde(default, alias = "expires_in_days")] expires_in_days: Option<i64>,
        #[serde(default, alias = "set_default")] set_default: Option<bool>,
        #[serde(default, alias = "apply_to_tools")] apply_to_tools: Option<bool>,
        #[serde(default, alias = "max_ratio_pct")] max_ratio_pct: Option<i32>,
    }
    // `expiresInDays` is three-valued, which `Option<Option<_>>` plus
    // `deserialize_with` is the only way to express: absent leaves the expiry
    // alone, an explicit `null` clears it, a number re-dates it from now.
    KeyUpdateArgs  {
        id: i64,
        #[serde(default)] label: Option<String>,
        #[serde(default)] enabled: Option<bool>,
        #[serde(default, alias = "expires_in_days", deserialize_with = "double_option")]
        expires_in_days: Option<Option<i64>>,
        #[serde(default, alias = "set_default")] set_default: Option<bool>,
        #[serde(default, alias = "apply_to_tools")] apply_to_tools: Option<bool>,
        #[serde(default, alias = "max_ratio_pct")] max_ratio_pct: Option<i32>,
    }
    KeyIdArgs      { id: i64 }
    ApiKeyArgs     {
        provider: String,
        #[serde(alias = "api_key")] api_key: String,
        /// Optional display name, so two keys on one vendor stay tellable apart.
        #[serde(default)] label: Option<String>,
    }
    ChainArgs      { chain: String }
    // `amount` absent or null → an open-ended session ("send any amount").
    PaySessionArgs { chain: String, #[serde(default)] amount: Option<i64> }
    // The card rail names no chain — there is no address to derive — and the
    // amount is not optional there: a card has to be presented with a figure.
    CardSessionArgs {
        #[serde(default)] amount: Option<i64>,
        #[serde(default, alias = "open_local")] open_local: bool,
    }
    // The gateway's own top-up. `amount` is required here, unlike the card
    // rail: a sealed order has to name a figure, and the server refuses one
    // that does not.
    PaygateSessionArgs {
        amount: i64,
        #[serde(default, alias = "open_local")] open_local: bool,
    }
    // The gateway's crypto panel. `withdraw` is the whole request, not a
    // detail of it — it decides whether a spending authorisation is minted.
    PaygatePanelArgs {
        #[serde(default)] withdraw: bool,
        #[serde(default, alias = "open_local")] open_local: bool,
    }
    PaySessionRefArgs { #[serde(alias = "session_ref")] session_ref: String }
    WithdrawArgs   {
        chain: String,
        #[serde(alias = "to_address")] to_address: String,
        amount: i64,
    }
    RecordsArgs    { #[serde(default)] role: Option<String>, #[serde(default)] page: Option<i64> }
    ModeArgs       { mode: String }
    // `models` absent → leave the selection unchanged; `[]` → clear it.
    BuyToolArgs    { tool: String, enabled: bool, #[serde(default)] models: Option<Vec<String>> }
    PathArgs       { path: String }
    // The agent firewall. `mode` absent on a switch-on falls back to `audit`.
    FirewallToolArgs { tool: String, enabled: bool, #[serde(default)] mode: Option<String> }
    FirewallEventsArgs { #[serde(default)] limit: Option<usize> }
    FirewallCheckArgs { text: String, #[serde(default)] kind: Option<String> }
    // The share card: a filename and the PNG itself, base64.
    SaveImageArgs  { name: String, data: String }
    // Every field past `enabled` absent → leave that term of the sale as it is.
    SellArgs       {
        provider: String,
        #[serde(alias = "account_id")] account_id: String,
        enabled: bool,
        #[serde(default, alias = "daily_limit")] daily_limit: Option<i64>,
        #[serde(default, alias = "min_ratio")] min_ratio: Option<i64>,
        #[serde(default, alias = "max_ratio")] max_ratio: Option<i64>,
        #[serde(default)] concurrency: Option<i64>,
        /// Which models this account sells: absent leaves the selection alone,
        /// `[]` puts every model it can serve back on the market.
        #[serde(default)] models: Option<Vec<String>>,
    }
    // Custom endpoints. Every term past the URL and key is optional and falls
    // back to the account's current value (or, on a first connect, to the
    // ordinary defaults) — `wire` to whatever the probe finds the endpoint
    // speaking.
    CustomEndpointArgs {
        #[serde(alias = "base_url")] base_url: String,
        #[serde(alias = "api_key")] api_key: String,
        #[serde(default)] wire: Option<String>,
        #[serde(default)] label: Option<String>,
        #[serde(default, alias = "min_ratio")] min_ratio: Option<i64>,
        #[serde(default)] concurrency: Option<i64>,
        #[serde(default)] enabled: Option<bool>,
    }
    EndpointArgs   { #[serde(alias = "account_id")] account_id: String }
    LaneArgs       {
        #[serde(default)] provider: Option<String>,
        #[serde(default, alias = "account_id")] account_id: Option<String>,
        #[serde(default)] model: Option<String>,
    }
    // The supply self-test. `wire` absent → the OpenAI-compatible dialect,
    // which is the one every buyer's tool can speak.
    ProbeArgs      {
        provider: String,
        #[serde(alias = "account_id")] account_id: String,
        model: String,
        #[serde(default)] wire: Option<String>,
    }
    // Model verification. Addressed by (provider, model) rather than by
    // account: the lane the market knows is `(device, provider, model)`, and
    // the device half is the daemon's own — a client that could name someone
    // else's device would be a way to spend a stranger's subscription.
    VerifyLaneArgs { provider: String, model: String }
    VerifyJobArgs  { #[serde(alias = "job_id")] job_id: String }
    PeriodArgs     { #[serde(default)] period: Option<String> }
    ForceArgs      { #[serde(default)] force: Option<bool> }
    OverviewArgs   { #[serde(default)] period: Option<String>, #[serde(default)] scope: Option<String> }
}

async fn rpc(
    State(ctx): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(cmd): Path<String>,
    headers: axum::http::HeaderMap,
    Json(a): Json<Value>,
) -> Response {
    if !authorized(&ctx, &peer, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(cmd_err!(
                "errors.daemon.unauthorized",
                "unauthorized (missing or bad X-Asale-Token)"
            )
            .to_json()),
        )
            .into_response();
    }
    let st = &ctx.state;

    // Every arm yields a `Value`; `?` inside the async block carries a
    // decode or command failure straight out as the error message. That is
    // what removes the `Err(e) => Err(e)` line each arm used to repeat.
    let out: Result<Value, commands::CmdError> = async {
        Ok(match cmd.as_str() {
        // ── no arguments ────────────────────────────────────────────────
        "client_config" => commands::client_config(st),
        "daemon_info" => commands::daemon_info(),
        "upgrade_notice" => commands::upgrade_notice(),
        "seller_status" => commands::seller_status(),
        "self_check" => serde_json::json!({"findings": crate::selfcheck::run(st).await}),
        "proxy_settings" => commands::proxy_settings(),
        "wallet_overview" => commands::wallet_overview(st).await?,
        "wallet_history" => commands::wallet_history(st).await?,
        "ensure_api_key" => commands::ensure_api_key(st).await?,
        "load_api_key" => commands::load_api_key(st).await.map(Value::Bool)?,
        "me_profile" => commands::me_profile(st).await?,
        "me_referral" => commands::me_referral(st).await?,
        "logout" => commands::logout(st).await.map(Value::Bool)?,
        "proxy_status" => commands::proxy_status(st).await?,
        "client_status" => commands::client_status(st).await?,
        "devices_list" => commands::devices_list(st).await?,
        "publish_policy_get" => commands::publish_policy_get(st).await?,
        "discovery_scan" => commands::discovery_scan().await?,
        "import_cli_all" => commands::import_cli_all(st).await?,
        "list_accounts" => commands::list_accounts(st).await?,
        "list_lanes" => commands::list_lanes(st).await?,
        "reconcile_now" => commands::reconcile_now(st).await?,
        "consume_get_mode" => commands::consume_get_mode(st).await?,
        "market_models" => commands::market_models(st).await?,
        "market_globe" => commands::market_globe(st).await?,
        "market_featured" => commands::market_featured(st).await?,
        "buy_tools" => commands::buy_tools(st).await?,
        // The agent firewall: the Security page reads its whole state here.
        "firewall_policy" => commands::firewall_policy(st).await?,
        // Which of those tools are running — the restart advice, made checkable.
        "tool_processes" => commands::tool_processes().await?,

        // ── upstream proxy + settings ───────────────────────────────────
        "set_proxy_settings" => {
            let p: ProxyArgs = args(&a)?;
            commands::set_proxy_settings(st, p.mode, p.url.unwrap_or_default()).await?
        },
        "test_proxy" => {
            let p: ProxyArgs = args(&a)?;
            commands::test_proxy(p.mode, p.url.unwrap_or_default()).await?
        },
        "selfcheck_fix" => {
            let p: SelfCheckFixArgs = args(&a)?;
            crate::selfcheck::fix(st, &p.id).await?
        },
        "get_setting" => {
            let p: GetSettingArgs = args(&a)?;
            commands::get_setting(st, p.key).await.map(|v| v.map_or(Value::Null, Value::String))?
        },
        "set_setting" => {
            let p: SetSettingArgs = args(&a)?;
            commands::set_setting(st, p.key, p.value).await.map(|_| Value::Bool(true))?
        },
        "consume_set_mode" => {
            let p: ModeArgs = args(&a)?;
            commands::consume_set_mode(st, p.mode).await?
        },

        // ── auth + profile ──────────────────────────────────────────────
        "register" => {
            let p: RegisterArgs = args(&a)?;
            commands::register(st, p.email, p.password, p.region).await?
        },
        "login" => {
            let p: CredentialArgs = args(&a)?;
            commands::login(st, p.email, p.password).await?
        },
        "resend_verification" => {
            let p: EmailArgs = args(&a)?;
            commands::resend_verification(st, p.email).await?
        },
        "update_profile" => {
            let p: ProfileArgs = args(&a)?;
            commands::update_profile(st, p.name, p.region, p.avatar_url).await?
        },
        "change_password" => {
            let p: PasswordArgs = args(&a)?;
            commands::change_password(st, p.old_password, p.new_password).await?
        },
        "unlink_oauth" => {
            let p: ProviderArgs = args(&a)?;
            commands::unlink_oauth(st, p.provider).await?
        },
        // ── consumer API keys ───────────────────────────────────────────
        "list_api_keys" => commands::list_api_keys(st).await?,
        "create_api_key" => {
            let p: KeyCreateArgs = args(&a)?;
            commands::create_api_key_ex(
                st,
                p.label.unwrap_or_else(|| "asale".into()),
                p.expires_in_days,
                p.set_default.unwrap_or(false),
                p.max_ratio_pct,
                p.apply_to_tools.unwrap_or(false),
            )
            .await?
        },
        "update_api_key" => {
            let p: KeyUpdateArgs = args(&a)?;
            commands::update_api_key(
                st,
                p.id,
                p.label,
                p.enabled,
                p.expires_in_days,
                p.set_default,
                p.max_ratio_pct,
                p.apply_to_tools.unwrap_or(false),
            )
            .await?
        },
        "delete_api_key" => {
            let p: KeyIdArgs = args(&a)?;
            commands::delete_api_key(st, p.id).await?
        },
        "reveal_api_key" => {
            let p: KeyIdArgs = args(&a)?;
            commands::reveal_api_key(st, p.id).await?
        },
        // Point this machine's buying tools at one key without changing which
        // key the account calls its default.
        "apply_api_key" => {
            let p: KeyIdArgs = args(&a)?;
            commands::apply_api_key(st, p.id).await?
        },

        // ── browser OAuth ───────────────────────────────────────────────
        "oauth_login" => {
            let p: OauthLoginArgs = args(&a)?;
            commands::oauth_login(st, p.provider, p.open_local.unwrap_or(false)).await?
        },
        "oauth_result" => {
            let p: OauthResultArgs = args(&a)?;
            commands::oauth_result(st, p.flow_id).await?
        },
        // The way out when the loopback callback cannot be reached: the UI is a
        // browser on another machine, so the redirect lands on *its* localhost
        // and the user pastes the address back here.
        "oauth_submit_code" => {
            let p: OauthCodeArgs = args(&a)?;
            commands::oauth_submit_code(st, p.flow_id, p.input).await?
        },
        "platform_oauth_login" => {
            let p: PlatformOauthArgs = args(&a)?;
            commands::platform_oauth_login(
                st,
                p.provider,
                p.link.unwrap_or(false),
                p.open_local.unwrap_or(false),
                p.region.unwrap_or_default(),
            )
            .await?
        },

        // ── accounts + lanes ────────────────────────────────────────────
        "import_from_cli" => {
            let p: ProviderArgs = args(&a)?;
            commands::import_from_cli(st, p.provider).await?
        },
        // Kimi Code / Grok CLI authorise by device code: no loopback callback,
        // so this one also works from a browser on another machine.
        "oauth_device_login" => {
            let p: OauthLoginArgs = args(&a)?;
            commands::oauth_device_login(st, p.provider, p.open_local.unwrap_or(false)).await?
        },
        // The metered platform APIs have no OAuth at all — the key is pasted.
        "connect_api_key" => {
            let p: ApiKeyArgs = args(&a)?;
            commands::connect_api_key(st, p.provider, p.api_key, p.label).await?
        },
        "remove_account" => {
            let p: AccountArgs = args(&a)?;
            commands::remove_account(st, p.provider, p.account_id).await.map(Value::Bool)?
        },
        // Sell side: one switch + daily cap + price band + concurrency ceiling,
        // per subscription account.
        "set_account_sell" => {
            let p: SellArgs = args(&a)?;
            commands::set_account_sell(
                st,
                p.provider,
                p.account_id,
                p.enabled,
                p.daily_limit,
                p.min_ratio,
                p.max_ratio,
                p.concurrency,
                p.models,
            )
            .await?
        },
        // An endpoint of its owner's own, sold as if it were a subscription.
        // Refused unless the server has granted this login the family — see
        // `commands::accounts::require_granted`.
        "connect_custom_endpoint" => {
            let p: CustomEndpointArgs = args(&a)?;
            commands::connect_custom_endpoint(
                st,
                p.base_url,
                p.api_key,
                p.wire,
                p.label,
                p.min_ratio,
                p.concurrency,
                p.enabled,
            )
            .await?
        },
        "list_custom_endpoints" => commands::list_custom_endpoints(st).await?,
        // Always answerable, so the UI can decide whether to offer the tab.
        // What the connect screen may draw, and the forms for it. Named for
        // what it answers rather than for one family — see
        // `commands::accounts::connect_offer`.
        "connect_offer" => commands::connect_offer(st).await?,
        // Kept under its old name for a frontend that has not been rebuilt.
        "custom_endpoints_status" => commands::connect_offer(st).await?,
        "refresh_custom_endpoint" => {
            let p: EndpointArgs = args(&a)?;
            commands::refresh_custom_endpoint(st, p.account_id).await?
        },
        "remove_custom_endpoint" => {
            let p: EndpointArgs = args(&a)?;
            commands::remove_custom_endpoint(st, p.account_id).await.map(Value::Bool)?
        },
        // Buy from this device's own lane, on purpose. Costs real money and
        // real subscription quota — see `commands::probe`.
        "test_supply" => {
            let p: ProbeArgs = args(&a)?;
            commands::test_supply(st, p.provider, p.account_id, p.model, p.wire.unwrap_or_default()).await?
        },
        // Model verification. Every one of these is a proxy — see
        // `commands::verify` for why the daemon deliberately decides nothing
        // here.
        "start_lane_verification" => {
            let p: VerifyLaneArgs = args(&a)?;
            commands::start_lane_verification(st, p.provider, p.model).await?
        },
        "lane_verification_job" => {
            let p: VerifyJobArgs = args(&a)?;
            commands::lane_verification_job(st, p.job_id).await?
        },
        "lane_verification_overview" => commands::lane_verification_overview(st).await?,
        "lane_verification_report" => {
            let p: VerifyLaneArgs = args(&a)?;
            commands::lane_verification_report(st, p.provider, p.model).await?
        },
        "resume_lane" => {
            let p: LaneArgs = args(&a)?;
            commands::resume_lane(
                st,
                p.provider.unwrap_or_default(),
                p.account_id.unwrap_or_default(),
                p.model.unwrap_or_default(),
            )
            .await?
        },
        // The publisher policy patch is a free-form object, forwarded as-is.
        "publish_policy_set" => commands::publish_policy_set(st, a.clone()).await?,

        // Buy side: one switch + model multi-select per locally installed CLI.
        "set_buy_tool" => {
            let p: BuyToolArgs = args(&a)?;
            commands::set_buy_tool(st, p.tool, p.enabled, p.models).await?
        },
        // Agent firewall: the per-tool switch, everything else on the policy,
        // the decision log, and the page's scratchpad.
        "set_firewall_tool" => {
            let p: FirewallToolArgs = args(&a)?;
            commands::set_firewall_tool(st, p.tool, p.enabled, p.mode).await?
        },
        "set_firewall_options" => commands::set_firewall_options(st, a.clone()).await?,
        "firewall_events" => {
            let p: FirewallEventsArgs = args(&a)?;
            commands::firewall_events(p.limit).await?
        },
        "firewall_check" => {
            let p: FirewallCheckArgs = args(&a)?;
            commands::firewall_check(st, p.text, p.kind).await?
        },

        // Open one of those tools' config files on the daemon's machine. Only
        // paths the buy switch itself writes are accepted — see the command.
        "open_config_path" => {
            let p: PathArgs = args(&a)?;
            commands::open_config_path(p.path).await?
        },
        "save_image" => {
            let p: SaveImageArgs = args(&a)?;
            commands::save_image(p.name, p.data).await?
        },

        // ── wallet ──────────────────────────────────────────────────────
        "wallet_deposit_address" => {
            let p: ChainArgs = args(&a)?;
            commands::wallet_deposit_address(st, p.chain).await?
        },
        "wallet_deposit_session" => {
            let p: PaySessionArgs = args(&a)?;
            commands::wallet_deposit_session(st, p.chain, p.amount).await?
        },
        // The card rail. No chain: there is no address, and no withdrawal.
        "wallet_card_session" => {
            let p: CardSessionArgs = args(&a)?;
            commands::wallet_card_session(st, p.amount, p.open_local).await?
        },
        // The gateway rails. Same two verbs, but the customer picks the
        // processor on a page of ours instead of us picking it for them.
        "wallet_paygate_session" => {
            let p: PaygateSessionArgs = args(&a)?;
            commands::wallet_paygate_session(st, p.amount, p.open_local).await?
        },
        "wallet_paygate_panel" => {
            let p: PaygatePanelArgs = args(&a)?;
            commands::wallet_paygate_panel(st, p.withdraw, p.open_local).await?
        },
        "wallet_deposit_session_get" => {
            let p: PaySessionRefArgs = args(&a)?;
            commands::wallet_deposit_session_get(st, p.session_ref).await?
        },
        "wallet_withdraw" => {
            let p: WithdrawArgs = args(&a)?;
            commands::wallet_withdraw(st, p.chain, p.to_address, p.amount).await?
        },

        // ── records + usage ─────────────────────────────────────────────
        "records_query" => {
            let p: RecordsArgs = args(&a)?;
            commands::records_query(st, p.role.unwrap_or_else(|| "provider".into()), p.page.unwrap_or(1)).await?
        },
        "usage_summary" => {
            let p: PeriodArgs = args(&a)?;
            commands::usage_summary(st, p.period.unwrap_or_else(|| "all".into())).await?
        },
        "usage_limits" => {
            let p: ForceArgs = args(&a)?;
            commands::usage_limits(st, p.force).await?
        },
        "usage_overview" => {
            let p: OverviewArgs = args(&a)?;
            commands::usage_overview(
                st,
                p.period.unwrap_or_else(|| "month".into()),
                p.scope.unwrap_or_else(|| "used".into()),
            )
            .await?
        },

        other => {
            return Err(cmd_err!(
                "errors.daemon.unknownCommand",
                format!("unknown command: {other}"),
                command = other
            ))
        }
        })
    }
    .await;

    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(e.to_json())).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the arg structs is that one `#[serde]` line replaces
    /// the two spellings every call site used to list by hand. If serde ever
    /// stopped accepting both, the frontend (camelCase) or the probes and older
    /// callers (snake_case) would start getting "bad arguments" — so pin it.
    fn headers(token: Option<&str>) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        if let Some(t) = token {
            h.insert("x-asale-token", t.parse().unwrap());
        }
        h
    }

    /// Being able to reach the port is not authorization. A local process with
    /// no token must not be able to read the wallet or flip the sell switches
    /// just because it can connect to 127.0.0.1.
    #[test]
    fn a_request_without_the_token_is_refused_whatever_its_source() {
        assert!(!token_matches("secret", &headers(None)));
        assert!(!token_matches("secret", &headers(Some("wrong"))));
        assert!(!token_matches("secret", &headers(Some(""))));
        assert!(token_matches("secret", &headers(Some("secret"))));
    }

    #[test]
    fn a_daemon_with_no_token_authorizes_nothing() {
        // Fail closed: an unreadable or empty token file must not degrade into
        // "everyone is allowed".
        assert!(!token_matches("", &headers(None)));
        assert!(!token_matches("", &headers(Some(""))));
        assert!(!cookie_matches("", &cookies("asale_session=")));
    }

    fn cookies(raw: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(header::COOKIE, raw.parse().unwrap());
        h
    }

    /// A browser proves itself with the cookie it was given at the gate, and
    /// the parse must not be fooled by a neighbouring cookie whose name merely
    /// contains ours, or by one that only looks like a prefix of the token.
    #[test]
    fn the_session_cookie_is_a_credential_in_its_own_right() {
        assert!(cookie_matches("secret", &cookies("asale_session=secret")));
        assert!(cookie_matches("secret", &cookies("theme=dark; asale_session=secret; tz=utc")));
        assert!(!cookie_matches("secret", &cookies("asale_session=wrong")));
        assert!(!cookie_matches("secret", &cookies("asale_session=sec")));
        assert!(!cookie_matches("secret", &cookies("xasale_session=secret")));
        assert!(!cookie_matches("secret", &cookies("theme=dark")));
    }

    /// The gate's own regression: deleting `?token=…` from the URL you were
    /// handed used to still serve the whole application. Whatever else changes
    /// here, a request carrying neither spelling of the token must not be one
    /// the UI answers with the app.
    #[test]
    fn a_url_with_the_token_stripped_off_does_not_reach_the_app() {
        let ctx_token = "secret";
        // No query, no cookie — the two ways in, both absent.
        assert!(query_token(&"/".parse::<Uri>().unwrap()).is_none());
        assert!(!cookie_matches(ctx_token, &axum::http::HeaderMap::new()));
    }

    #[test]
    fn the_tokenized_url_is_recognized_in_the_forms_that_are_handed_out() {
        let q = |u: &str| query_token(&u.parse::<Uri>().unwrap());
        assert_eq!(q("/?token=abc").as_deref(), Some("abc"));
        assert_eq!(q("/settings?token=abc").as_deref(), Some("abc"));
        // Not the first parameter, and not the only one.
        assert_eq!(q("/?lang=zh&token=abc").as_deref(), Some("abc"));
        assert_eq!(q("/?token=abc&lang=zh").as_deref(), Some("abc"));
        // A fragment never reaches a server, so this is genuinely tokenless
        // here — the unlock page is what reads that spelling.
        assert_eq!(q("/#token=abc"), None);
        assert_eq!(q("/?tokenish=abc"), None);
        assert_eq!(q("/"), None);
    }

    /// The cookie is the token's stand-in, so its attributes are the security
    /// properties: unreadable to scripts, never sent by another site, and not
    /// permanent.
    #[test]
    fn the_session_cookie_carries_the_attributes_that_make_it_safe() {
        let c = session_cookie("secret");
        assert!(c.starts_with("asale_session=secret;"), "{c}");
        assert!(c.contains("HttpOnly"), "{c}");
        assert!(c.contains("SameSite=Strict"), "{c}");
        assert!(c.contains(&format!("Max-Age={SESSION_MAX_AGE_SECS}")), "{c}");
        // `Secure` would make the browser drop it: the daemon speaks http.
        assert!(!c.contains("Secure"), "{c}");
    }

    #[test]
    fn the_desktop_shell_is_admitted_on_every_platform_it_ships_for() {
        // Windows is the one that bit: WebView2 has no custom-scheme support,
        // so the packaged app is served over http there while macOS and Linux
        // get the custom scheme. Missing the http spelling meant every RPC from
        // a packaged Windows client was blocked before it was ever authorized,
        // and the app reported the daemon as not running.
        assert!(allowed_origin("tauri://localhost"), "macOS / Linux");
        assert!(allowed_origin("http://tauri.localhost"), "Windows (WebView2)");
        assert!(allowed_origin("https://tauri.localhost"));
        // A browser on this machine, at the dev server or the daemon's own port.
        assert!(allowed_origin("http://localhost:9173"));
        assert!(allowed_origin("http://127.0.0.1:9700"));
        // Anything else is a page that found the port, and the token is not the
        // only thing that should be standing between it and the wallet.
        assert!(!allowed_origin("https://evil.example"));
        assert!(!allowed_origin("http://tauri.localhost.evil.example"));
        assert!(!allowed_origin("http://localhost.evil.example"));
    }

    #[test]
    fn multi_word_arguments_are_accepted_in_both_spellings() {
        let camel: AccountArgs = args(&json!({"provider": "claude", "accountId": "a@b.io"})).unwrap();
        let snake: AccountArgs = args(&json!({"provider": "claude", "account_id": "a@b.io"})).unwrap();
        assert_eq!(camel.account_id, "a@b.io");
        assert_eq!(snake.account_id, "a@b.io");

        let camel: WithdrawArgs =
            args(&json!({"chain": "tron", "toAddress": "T1", "amount": 5})).unwrap();
        let snake: WithdrawArgs =
            args(&json!({"chain": "tron", "to_address": "T1", "amount": 5})).unwrap();
        assert_eq!((camel.to_address.as_str(), camel.amount), ("T1", 5));
        assert_eq!((snake.to_address.as_str(), snake.amount), ("T1", 5));

        let camel: OauthResultArgs = args(&json!({"flowId": "f1"})).unwrap();
        let snake: OauthResultArgs = args(&json!({"flow_id": "f1"})).unwrap();
        assert_eq!(camel.flow_id, snake.flow_id);

        let camel: SellArgs =
            args(&json!({"provider": "claude", "accountId": "a", "enabled": true, "dailyLimit": 9})).unwrap();
        let snake: SellArgs =
            args(&json!({"provider": "claude", "account_id": "a", "enabled": true, "daily_limit": 9})).unwrap();
        assert_eq!(camel.daily_limit, Some(9));
        assert_eq!(snake.daily_limit, Some(9));

        let camel: KeyCreateArgs =
            args(&json!({"label": "ci", "expiresInDays": 30, "applyToTools": true})).unwrap();
        let snake: KeyCreateArgs =
            args(&json!({"label": "ci", "expires_in_days": 30, "apply_to_tools": true})).unwrap();
        assert_eq!((camel.expires_in_days, camel.apply_to_tools), (Some(30), Some(true)));
        assert_eq!((snake.expires_in_days, snake.apply_to_tools), (Some(30), Some(true)));
    }

    /// An API key's expiry is three-valued on the wire, and the whole point of
    /// `double_option` is that the middle value survives the parse: the page
    /// sends one field at a time, so "leave the expiry alone" and "clear it"
    /// have to arrive as different things.
    #[test]
    fn an_absent_key_expiry_is_not_a_null_one() {
        let leave: KeyUpdateArgs = args(&json!({"id": 1, "label": "x"})).unwrap();
        assert_eq!(leave.expires_in_days, None, "absent = leave it alone");

        let clear: KeyUpdateArgs = args(&json!({"id": 1, "expiresInDays": null})).unwrap();
        assert_eq!(clear.expires_in_days, Some(None), "null = never expires");

        let set: KeyUpdateArgs = args(&json!({"id": 1, "expires_in_days": 90})).unwrap();
        assert_eq!(set.expires_in_days, Some(Some(90)));

        // The frontend spells this one `setDefault`; the daemon's own probes
        // and curl-by-hand use the snake form.
        let camel: KeyUpdateArgs = args(&json!({"id": 7, "setDefault": true})).unwrap();
        let snake: KeyUpdateArgs = args(&json!({"id": 7, "set_default": true})).unwrap();
        assert_eq!((camel.id, camel.set_default), (7, Some(true)));
        assert_eq!((snake.id, snake.set_default), (7, Some(true)));
    }

    #[test]
    fn absent_and_null_optionals_both_mean_unset() {
        // The old `get2` helper filtered nulls explicitly; `Option` does it for
        // free, and the buy page relies on the distinction: no `models` key
        // leaves the selection alone, `[]` clears it.
        let absent: BuyToolArgs = args(&json!({"tool": "claude", "enabled": true})).unwrap();
        assert_eq!(absent.models, None);
        let null: BuyToolArgs =
            args(&json!({"tool": "claude", "enabled": true, "models": null})).unwrap();
        assert_eq!(null.models, None);
        let cleared: BuyToolArgs =
            args(&json!({"tool": "claude", "enabled": true, "models": []})).unwrap();
        assert_eq!(cleared.models, Some(vec![]));
    }

    #[test]
    fn a_missing_required_argument_is_a_readable_error() {
        let e = args::<AccountArgs>(&json!({"provider": "claude"})).unwrap_err();
        assert!(e.message.contains("accountId"), "unhelpful message: {e}");
        // Decode failures are translatable too, so the frontend does not have
        // to show serde's English at a user.
        assert_eq!(e.key.as_deref(), Some("errors.daemon.badArguments"));
    }

    #[test]
    fn unknown_extra_keys_are_ignored() {
        // The frontend sends `openLocal` to every command via a shared wrapper.
        let p: ProviderArgs = args(&json!({"provider": "claude", "openLocal": true})).unwrap();
        assert_eq!(p.provider, "claude");
    }
}
