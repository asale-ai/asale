//! HTTP surface of the daemon:
//!
//!   POST /rpc/{cmd}   — JSON-RPC-ish command dispatch onto `commands::*`.
//!                       Body = the command's args object (camelCase keys, the
//!                       same shape the old Tauri `invoke` used). 200 → result
//!                       JSON; 4xx/5xx → `{"error": "..."}`.
//!   GET  /healthz     — liveness (also used by the shell to detect a running
//!                       daemon and by the frontend to detect the daemon).
//!   GET  /*           — the embedded web UI (../dist), so one `asaled` binary
//!                       serves the whole app for B/S access.
//!
//! Auth model:
//!   - **Every** /rpc request must carry `X-Asale-Token: <token>` matching
//!     `~/.asale/daemon.token` (0600, generated on first run) — loopback
//!     included. See `authorized` for what that does and does not protect.
//!   - The Tauri shell reads the file and seeds the webview's localStorage in
//!     an initialization script; browsers use the `#token=` URL the daemon
//!     prints at startup (the frontend captures it into localStorage on first
//!     load and scrubs it from the address bar).
//!   - `/healthz` and the static UI stay open: the first is a liveness probe
//!     with no data in it, the second has to load before it can present a
//!     token.
//!   - CSRF: /rpc only accepts `Content-Type: application/json` (axum's `Json`
//!     rejects others), so cross-origin browser calls always trigger a CORS
//!     preflight, and the CORS layer only admits known origins.

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

/// Serve the embedded UI; unknown paths fall back to index.html (the app is a
/// single page). An empty dist (API-only build) returns a plain hint instead.
async fn ui(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
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
                    "default-src 'self'; img-src 'self' data: https:; style-src 'self' 'unsafe-inline'; \
                     script-src 'self'; connect-src 'self' http://127.0.0.1:* http://localhost:* \
                     ws://127.0.0.1:* ws://localhost:*; frame-ancestors 'none'; base-uri 'none'",
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
    token_matches(&ctx.token, headers)
}

/// The whole authorization decision, as a pure function over the expected token
/// and the request headers. An empty expected token authorizes nothing.
fn token_matches(expected: &str, headers: &axum::http::HeaderMap) -> bool {
    if expected.is_empty() {
        return false;
    }
    headers
        .get("x-asale-token")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()))
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
    CredentialArgs { email: String, password: String }
    // Registration takes the country the sign-up screen collected; the server
    // requires it, and it is the only moment the platform can learn one.
    RegisterArgs   { email: String, password: String, #[serde(default)] region: String }
    EmailArgs      { email: String }
    LabelArgs      { #[serde(default)] label: Option<String> }
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
    ApiKeyArgs     {
        provider: String,
        #[serde(alias = "api_key")] api_key: String,
        /// Optional display name, so two keys on one vendor stay tellable apart.
        #[serde(default)] label: Option<String>,
    }
    ChainArgs      { chain: String }
    // `amount` absent or null → an open-ended session ("send any amount").
    PaySessionArgs { chain: String, #[serde(default)] amount: Option<i64> }
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
    // Every field past `enabled` absent → leave that term of the sale as it is.
    SellArgs       {
        provider: String,
        #[serde(alias = "account_id")] account_id: String,
        enabled: bool,
        #[serde(default, alias = "daily_limit")] daily_limit: Option<i64>,
        #[serde(default, alias = "min_ratio")] min_ratio: Option<i64>,
        #[serde(default, alias = "max_ratio")] max_ratio: Option<i64>,
        #[serde(default)] concurrency: Option<i64>,
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

        // ── upstream proxy + settings ───────────────────────────────────
        "set_proxy_settings" => {
            let p: ProxyArgs = args(&a)?;
            commands::set_proxy_settings(st, p.mode, p.url.unwrap_or_default()).await?
        },
        "test_proxy" => {
            let p: ProxyArgs = args(&a)?;
            commands::test_proxy(p.mode, p.url.unwrap_or_default()).await?
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
        "create_api_key" => {
            let p: LabelArgs = args(&a)?;
            commands::create_api_key(st, p.label.unwrap_or_else(|| "asale".into())).await?
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
            )
            .await?
        },
        // An endpoint of the operator's own, sold as if it were a subscription.
        // On unless the client was built with ASALE_CUSTOM_ENDPOINTS=0 — see
        // `commands::accounts`.
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
        "custom_endpoints_status" => commands::custom_endpoints_status(st).await?,
        "refresh_custom_endpoint" => {
            let p: EndpointArgs = args(&a)?;
            commands::refresh_custom_endpoint(st, p.account_id).await?
        },
        "remove_custom_endpoint" => {
            let p: EndpointArgs = args(&a)?;
            commands::remove_custom_endpoint(st, p.account_id).await.map(Value::Bool)?
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
        // Open one of those tools' config files on the daemon's machine. Only
        // paths the buy switch itself writes are accepted — see the command.
        "open_config_path" => {
            let p: PathArgs = args(&a)?;
            commands::open_config_path(p.path).await?
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
