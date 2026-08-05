//! Consumer local proxy (spec §6). Listens on 127.0.0.1:<port> and exposes the
//! compatible endpoints. Route decision (spec §6.1):
//!
//!   direct → forward to the official upstream with a locally imported
//!            subscription token (pool-selected, injected here only; no trade,
//!            never touches the asale server).
//!   market → inject the asale API key (kept in the encrypted secret store,
//!            never in the tool config) and forward to the asale server gateway.
//!   auto   → direct when a local account with remaining quota can serve the
//!            request's dialect natively; market otherwise.
//!
//! Market forwards write a local `consume_records` row (spec §8) with the
//! usage parsed from the streamed response, for reconciliation.

use asale_client_core::executor::UsageScanner;
use asale_client_core::protocol::Usage;
use asale_client_core::store::LocalStore;
use asale_client_core::{AccountPool, UpstreamErrorKind};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use futures_util::StreamExt;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct ProxyState {
    pub server_api_base: String, // http(s)://host:port (asale gateway API)
    pub asale_key: Arc<tokio::sync::RwLock<Option<String>>>,
    pub http: reqwest::Client,
    pub store: Arc<LocalStore>,
    pub pool: Arc<StdMutex<AccountPool>>,
    /// The daemon state, for the paths that need more than the proxy's own
    /// slice of it — currently only re-minting a rejected API key, which needs
    /// the session tokens. `None` in tests, where nothing is minted.
    pub app: Option<Arc<crate::state::AppState>>,
    /// Serializes those re-mints; see `remint_key`.
    pub remint_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Build the proxy router.
pub fn router(state: ProxyState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(forward))
        .route("/v1/completions", post(forward))
        // Codex ≥ 0.146 rejects `wire_api = "chat"` outright, so the whole
        // Codex family reaches us here and nowhere else.
        .route("/v1/responses", post(forward))
        .route("/v1/messages", post(forward))
        // Claude Code's token-count preflight. Forwarded like a message call but
        // never metered/settled (it is not a billable completion).
        .route("/v1/messages/count_tokens", post(forward))
        .route("/v1/models", get(forward))
        .route("/v1beta/models/*rest", post(forward))
        // Per-tool addressing. A tool whose dialect cannot identify it is
        // pointed at `<proxy>/{tool}` instead of the bare origin, and the
        // leading segment is what `tool_config::for_request_path` reads. These
        // never shadow the bare forms above: those are shorter, and a static
        // segment outranks `:tool` where the two could both match.
        .route("/:tool/v1/messages", post(forward))
        .route("/:tool/v1/messages/count_tokens", post(forward))
        .route("/:tool/v1/chat/completions", post(forward))
        .route("/:tool/v1/completions", post(forward))
        .route("/:tool/v1/responses", post(forward))
        .route("/:tool/v1/models", get(forward))
        .route("/:tool/v1beta/models/*rest", post(forward))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

/// Largest request body this proxy will buffer.
///
/// Deliberately above the asale gateway's own default
/// (`ASALE_MAX_REQUEST_BYTES`, 100 MiB) so that an oversized market request is
/// refused by the gateway with a 413 the caller can read, rather than dying
/// here as an opaque local error. The direct route bypasses the gateway
/// entirely and is bounded only by this.
const MAX_BODY_BYTES: usize = 128 * 1024 * 1024;

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

use crate::tool_config::for_request_path as buy_tool_for_path;

/// Segments that tag a build or environment rather than name a model.
const MODEL_TAGS: &[&str] = &["latest", "preview", "exp", "e2e", "test", "dev", "staging", "canary"];

/// A model id without the release stamp the market appends:
/// `claude-fable-5-e2e-1784891711622250000` → `claude-fable-5`,
/// `claude-opus-4-8-20260101` → `claude-opus-4-8`. Ids that carry no such
/// suffix (`claude-3-5-haiku`) are returned whole.
///
/// Named `strip_release_stamp`, not `model_family`: the server's
/// `metering::model_family` is an unrelated function that classifies an id into
/// claude/gpt for token-estimation heuristics, and having both under one name
/// made the two look like copies of each other that had drifted.
pub fn strip_release_stamp(id: &str) -> &str {
    let mut end = id.len();
    while let Some(dash) = id[..end].rfind('-') {
        // Never strip down to a single segment.
        if !id[..dash].contains('-') {
            break;
        }
        let seg = &id[dash + 1..end];
        let stamp = seg.len() >= 6 && seg.bytes().all(|b| b.is_ascii_digit());
        let tag = MODEL_TAGS.iter().any(|t| seg.eq_ignore_ascii_case(t));
        if !stamp && !tag {
            break;
        }
        end = dash;
    }
    &id[..end]
}

/// What the buy gate decided about a request.
enum BuyDecision {
    /// Forward as it stands.
    Pass,
    /// Do not forward; answer the caller with this instead.
    Refuse(Response),
    /// Forward, but relabel the request with this model first.
    Substitute(String),
}

/// Gate a request on the originating tool's buy settings.
///
/// A tool whose switch is off should not be pointing at us at all — its config
/// was restored — so that case only fires for a stale process still holding the
/// old endpoint. Refusing beats silently spending the user's balance.
async fn buy_gate(st: &ProxyState, tool: Option<&str>, model: &str) -> BuyDecision {
    let Some(tool) = tool else { return BuyDecision::Pass };
    if !crate::commands::buy_is_enabled(&st.store, tool).await {
        return BuyDecision::Refuse(
            (
                StatusCode::FORBIDDEN,
                format!("buying is off for {tool} — turn it on in Asale to route this tool through the market"),
            )
                .into_response(),
        );
    }
    // An empty selection means "any model the market offers". The UI stores a
    // model *family* (`claude-fable-5`), while callers ask for a full release id
    // (`claude-fable-5-e2e-1784891711622250000`), so compare on the family.
    let allowed = crate::commands::buy_models(&st.store, tool).await;
    let in_buy_list =
        |id: &str| allowed.iter().any(|m| strip_release_stamp(m) == strip_release_stamp(id));

    // Codex's picker can only offer slugs OpenAI already ships, so the models
    // the user bought are published under native ones and named here instead.
    // Resolve the slug the picker sent back to the model it stands for — but
    // only while the buy list still agrees, so a stale alias table left behind
    // by an earlier selection cannot smuggle a model past the gate.
    if tool == "codex" {
        if let Some(bought) = crate::codex_catalog::alias_for(model) {
            if allowed.is_empty() || in_buy_list(&bought) {
                return BuyDecision::Substitute(bought);
            }
        }
    }

    if !allowed.is_empty() && !model.is_empty() && !in_buy_list(model) {
        // No tool can be told to ask for the model that was bought. Their
        // pickers only offer their own vendor's catalog (Claude Code lists
        // Anthropic ids, Codex OpenAI ones), and on top of that both run
        // app-internal work — thread titling, auto-review, compaction — against
        // ids baked into the binary (`gpt-5.6-luna` and friends) that no picker
        // or config key can redirect. Meanwhile the buy picker offers the whole
        // market catalog to every tool, so "Claude Code + gpt-5.1" is a
        // selection the user is invited to make and the caller can never
        // satisfy. Refusing would break the tool over a request it never chose
        // to send; serve everything with the model that was actually bought
        // instead — the gateway translates dialects (spec §5), so a Claude Code
        // session runs fine on a gpt model.
        return BuyDecision::Substitute(allowed[0].clone());
    }
    BuyDecision::Pass
}

/// Providers whose native dialect matches this local path — candidates for
/// direct routing. Claude Code and Claude Work accounts share the Anthropic
/// dialect; codex OAuth tokens have no public compat endpoint, so codex paths
/// stay on the market route.
fn direct_candidates(path: &str) -> &'static [&'static str] {
    if path.starts_with("/v1/messages") {
        &["claude", "claude_work"]
    } else if path.starts_with("/v1beta/") {
        &["gemini"]
    } else {
        &[]
    }
}

/// The official upstream URL + extra headers for a direct forward. `path` is the
/// request path (e.g. `/v1/messages` or `/v1/messages/count_tokens`) so the same
/// forwarder serves both message calls and the count_tokens preflight.
fn direct_upstream(provider: &str, path: &str, path_and_query: &str) -> Option<(String, Vec<(&'static str, String)>)> {
    match provider {
        "claude" | "claude_work" => Some((
            format!("https://api.anthropic.com{path}"),
            vec![
                ("anthropic-version", "2023-06-01".to_string()),
                // OAuth (subscription) tokens require the oauth beta flag.
                ("anthropic-beta", "oauth-2025-04-20".to_string()),
                (
                    "user-agent",
                    if provider == "claude_work" { "claude-work/1.0 (desktop)" } else { "claude-cli/1.0 (external, cli)" }
                        .to_string(),
                ),
            ],
        )),
        "gemini" => Some((
            format!("https://generativelanguage.googleapis.com{path_and_query}"),
            vec![("user-agent", "gemini-cli/1.0".to_string())],
        )),
        _ => None,
    }
}

/// Resolve the effective route for this request: `Some(provider)` → direct.
async fn decide_direct(st: &ProxyState, path: &str) -> Option<&'static str> {
    let mode = st
        .store
        .get_setting("consume_mode")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "market".to_string());
    if mode == "market" {
        return None;
    }
    let now = now_secs();
    let candidates = direct_candidates(path);
    let hit = {
        let pool = st.pool.lock().ok()?;
        candidates.iter().find(|p| pool.any_available(p, now)).copied()
    };
    // mode=direct with no local account still routes direct (and errors
    // explicitly) so the user sees why; auto silently falls back to market.
    match (mode.as_str(), hit) {
        ("direct", Some(p)) => Some(p),
        ("direct", None) => Some(candidates.first().copied().unwrap_or("")),
        ("auto", p) => p,
        _ => None,
    }
}

async fn forward(
    State(st): State<ProxyState>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    _headers: HeaderMap,
    body: Body,
) -> Response {
    // The tool is read from the *raw* path — the `/{tool}` prefix is the only
    // thing that carries it — and everything downstream sees the path without
    // it, because that prefix is ours and means nothing to the gateway.
    let raw_path = uri.path();
    let tool = buy_tool_for_path(raw_path);
    // The `/:tool/...` routes match *any* leading segment, so a prefix naming
    // no tool we know lands here with nothing to gate on — and `buy_gate`
    // passes a `None` tool through. Refuse it: the alternative is a request
    // that spends the user's balance without any switch having been turned on.
    let head = raw_path.trim_start_matches('/').split('/').next().unwrap_or("");
    if tool.is_none() && !head.starts_with("v1") {
        return (StatusCode::NOT_FOUND, format!("unknown tool prefix `/{head}`")).into_response();
    }
    let path = crate::tool_config::strip_tool_prefix(raw_path).to_string();
    let path_and_query = uri
        .path_and_query()
        .map(|pq| crate::tool_config::strip_tool_prefix(pq.as_str()).to_string())
        .unwrap_or_else(|| "/".into());

    let mut bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "body read error").into_response(),
    };

    // Only meter billable model-call POSTs — not /v1/models listings, and not
    // the count_tokens preflight (which has no usage to settle).
    let is_count_tokens = path.ends_with("/count_tokens");
    let meter = method == axum::http::Method::POST && !is_count_tokens;

    // Enforce the originating tool's buy switch + model selection. count_tokens
    // is a preflight Claude Code issues before the real call; gating it too
    // would surface the refusal at a confusing point, and it costs nothing.
    if !is_count_tokens {
        match buy_gate(&st, tool, &extract_model_from_bytes(&bytes)).await {
            BuyDecision::Pass => {}
            BuyDecision::Refuse(refusal) => return refusal,
            BuyDecision::Substitute(model) => bytes = relabel_model(&bytes, &model),
        }
    }

    if let Some(provider) = decide_direct(&st, &path).await {
        if provider.is_empty() {
            return (StatusCode::SERVICE_UNAVAILABLE, "consume mode is 'direct' but no local subscription can serve this endpoint").into_response();
        }
        return forward_direct(st, provider, &path, &path_and_query, method, bytes, meter).await;
    }
    forward_market(st, &path_and_query, method, bytes, meter).await
}

/// Direct route: inject the pool-selected local subscription token and call the
/// official upstream. No trade, no asale server involvement (spec §6.1).
async fn forward_direct(
    st: ProxyState,
    provider: &'static str,
    path: &str,
    path_and_query: &str,
    method: axum::http::Method,
    bytes: axum::body::Bytes,
    meter: bool,
) -> Response {
    // Lane-aware pick: a model that just failed upstream is backed off for
    // local traffic too, while a *market* pause (breaker, sell switch) is not
    // allowed to lock the operator out of their own subscription.
    let model = extract_model_from_bytes(&bytes);
    let picked = match st.pool.lock().ok().and_then(|mut p| p.pick_local(provider, &model, now_secs())) {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "consume mode is 'direct' but no local account is available (cooldown/expired/exhausted)",
            )
                .into_response();
        }
    };
    let token = match crate::keychain::get(&picked.keychain_ref).ok().flatten() {
        Some(t) => t,
        None => {
            if let Ok(mut pool) = st.pool.lock() {
                pool.on_error(provider, &picked.account_id, &model, UpstreamErrorKind::AuthFailed, "missing credential", now_secs());
            }
            return (StatusCode::SERVICE_UNAVAILABLE, "local account token missing from secret store").into_response();
        }
    };
    let Some((url, extra_headers)) = direct_upstream(provider, path, path_and_query) else {
        return (StatusCode::BAD_GATEWAY, "provider has no direct upstream").into_response();
    };

    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST);
    // Direct mode leaves asale entirely and hits the provider, so it needs the
    // proxy-aware client — `st.http` is for the (unproxied) asale gateway.
    let mut req = asale_client_core::http::upstream()
        .request(reqwest_method, &url)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    for (k, v) in extra_headers {
        req = req.header(k, v);
    }
    // Same OAuth requirement the relay executor applies: Anthropic refuses
    // subscription traffic that does not open with Claude Code's preamble, and
    // dresses the refusal as a 429. Claude Code's own body already carries it
    // (this is a no-op then); any other Anthropic-dialect caller would not.
    let mut out_body = bytes.to_vec();
    if provider == "claude" || provider == "claude_work" {
        if let Some(patched) = asale_client_core::executor::with_claude_code_system(&out_body) {
            out_body = patched;
        }
    }
    let resp = match req.body(out_body).send().await {
        Ok(r) => r,
        Err(e) => {
            if let Ok(mut pool) = st.pool.lock() {
                pool.on_error(provider, &picked.account_id, &model, UpstreamErrorKind::ServerError, &e.to_string(), now_secs());
            }
            return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response();
        }
    };

    let status = resp.status();
    // Pool feedback mirrors the executor's policy (spec §4).
    if !status.is_success() {
        let kind = match status.as_u16() {
            429 => Some(UpstreamErrorKind::RateLimited { reset_at: None }),
            401 | 403 => Some(UpstreamErrorKind::AuthFailed),
            s if s >= 500 => Some(UpstreamErrorKind::ServerError),
            _ => None,
        };
        if let Ok(mut pool) = st.pool.lock() {
            match kind {
                Some(k) => {
                    pool.on_error(provider, &picked.account_id, &model, k, &format!("upstream {status}"), now_secs());
                }
                None => pool.on_success(provider, &picked.account_id, &model, 0),
            }
        }
        if meter {
            let task_id = format!("d_{}", uuid::Uuid::new_v4().simple());
            let _ = st
                .store
                .insert_consume_record(&task_id, &model, 0, 0, 0, &format!("direct_upstream_{}", status.as_u16()))
                .await;
        }
        return tag_direct(passthrough(resp), provider, &model);
    }

    // Success: stream through, metering usage; release the pool lease with the
    // measured tokens when the stream ends.
    let pool = st.pool.clone();
    let account_id = picked.account_id.clone();
    let store = st.store.clone();
    let lane = model.clone();
    let tag = model.clone();
    let record = meter.then(|| (format!("d_{}", uuid::Uuid::new_v4().simple()), model, "direct".to_string()));
    let served = meter_response(resp, move |usage, had_error| {
        let tokens = (usage.input_tokens + usage.output_tokens).max(0) as u64;
        if let Ok(mut p) = pool.lock() {
            if had_error {
                p.on_error(provider, &account_id, &lane, UpstreamErrorKind::ServerError, "stream aborted", now_secs());
            } else {
                p.on_success(provider, &account_id, &lane, tokens);
            }
        }
        let store = store.clone();
        let record = record.clone();
        async move {
            if let Some((task_id, model, status)) = record {
                let status = if had_error { "stream_error".to_string() } else { status };
                let _ = store
                    .insert_consume_record(&task_id, &model, usage.input_tokens, usage.output_tokens, 0, &status)
                    .await;
            }
        }
    })
    .await;
    tag_direct(served, provider, &tag)
}

/// Tag a direct-route answer the same way the gateway tags a market one.
///
/// Direct never reaches the gateway, so there are no headers to copy — but a
/// tool that sees provenance on some answers and none on others learns nothing
/// from either. `self = 1` is not a judgement here, it is the definition: the
/// direct route *is* the user's own subscription.
fn tag_direct(mut resp: Response, provider: &str, model: &str) -> Response {
    let headers = resp.headers_mut();
    for (k, v) in [("x-asale-upstream", provider), ("x-asale-source", "direct"), ("x-asale-model", model), ("x-asale-self", "1")] {
        if let Ok(value) = axum::http::HeaderValue::from_str(v) {
            headers.insert(k, value);
        }
    }
    resp
}

/// One market forward. Split out so the 401 self-heal below can replay the
/// exact same request under a fresh key.
async fn send_market(
    st: &ProxyState,
    target: &str,
    method: &reqwest::Method,
    bytes: &axum::body::Bytes,
    key: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req = st
        .http
        .request(method.clone(), target)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    // The gateway answers a few refusals — "upgrade the app", "nobody is selling
    // this", "top up" — as an assistant turn inside the user's AI session rather
    // than as a status code their tool prints raw. It writes that sentence in
    // the language named here, so a user who set the app to Chinese does not get
    // told to upgrade in English.
    if let Some(lang) = ui_language(st).await {
        req = req.header("accept-language", lang);
    }
    req.body(bytes.to_vec()).send().await
}

/// The language the user picked in the app, if they picked one.
///
/// Same `language` setting the desktop shell and the tray read, so all three
/// agree. A miss is not worth a log line: the gateway falls back to the
/// deployment's own default, which is a reasonable answer and not an error.
async fn ui_language(st: &ProxyState) -> Option<String> {
    st.store.get_setting("language").await.ok().flatten().filter(|v| !v.trim().is_empty())
}

/// Replace the key the gateway just rejected.
///
/// Serialized on `remint_lock`, and re-checked inside it: a tool in flight
/// fires several requests at once, and all of them would otherwise mint a key
/// of their own, leaving the account littered with keys nothing uses.
async fn remint_key(st: &ProxyState, used: &str) -> Result<String, String> {
    let app = st.app.as_ref().ok_or("no daemon state to mint a key with")?;
    let _guard = st.remint_lock.lock().await;
    if let Some(current) = app.asale_key.read().await.clone() {
        if current != used {
            return Ok(current); // another request already replaced it
        }
    }
    crate::commands::mint_consumer_key(app).await.map_err(|e| e.message)
}

/// Market route: forward to the asale server gateway with the asale API key,
/// recording a consume row from the streamed usage (spec §6.2/§8).
async fn forward_market(
    st: ProxyState,
    path_and_query: &str,
    method: axum::http::Method,
    bytes: axum::body::Bytes,
    meter: bool,
) -> Response {
    let mut key = match st.asale_key.read().await.clone() {
        Some(k) => k,
        None => {
            return (StatusCode::UNAUTHORIZED, "Asale api key not configured").into_response();
        }
    };
    let target = format!("{}{}", st.server_api_base.trim_end_matches('/'), path_and_query);
    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST);
    let model = extract_model_from_bytes(&bytes);

    let mut resp = match send_market(&st, &target, &reqwest_method, &bytes, &key).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response(),
    };

    // The gateway does not know this key. Retrying it never helps — the key row
    // is gone from the server (deployment switched, database rebuilt, keys
    // revoked) — so mint a replacement and send the request once more. Without
    // this the tool just reports `401 unknown api key` forever, and the only
    // cure is a "regenerate key" click nothing on screen asks for.
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        match remint_key(&st, &key).await {
            Ok(fresh) => {
                key = fresh;
                resp = match send_market(&st, &target, &reqwest_method, &bytes, &key).await {
                    Ok(r) => r,
                    Err(e) => return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response(),
                };
            }
            Err(e) => tracing::warn!("gateway rejected the asale api key and re-minting failed: {e}"),
        }
    }

    let status = resp.status();
    log_provenance(resp.headers());
    // The gateway normally answers this one as an assistant turn the user reads
    // in their AI session, so it arrives here as a 200 and never trips this
    // branch — but a caller the gateway does not recognise as our client (or an
    // older gateway) still gets the real 426, and either way the desktop window
    // should be showing an upgrade button.
    let notice = resp.headers().get("x-asale-notice").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if status == reqwest::StatusCode::UPGRADE_REQUIRED || notice == "client_upgrade_required" {
        let min = resp
            .headers()
            .get("x-asale-notice-min-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        asale_client_core::upgrade::record(min, "buy");
    } else if status.is_success() && notice.is_empty() {
        // A trade that actually completed — not a refusal wearing a 200 — is the
        // proof that this build is still acceptable.
        asale_client_core::upgrade::clear();
    }
    if !status.is_success() {
        if meter {
            let task_id = format!("c_{}", uuid::Uuid::new_v4().simple());
            let _ = st
                .store
                .insert_consume_record(&task_id, &model, 0, 0, 0, &format!("upstream_{}", status.as_u16()))
                .await;
        }
        return passthrough(resp);
    }

    let store = st.store.clone();
    let record = meter.then(|| (format!("c_{}", uuid::Uuid::new_v4().simple()), model));
    meter_response(resp, move |usage, had_error| {
        let store = store.clone();
        let record = record.clone();
        async move {
            if let Some((task_id, model)) = record {
                let status = if had_error { "stream_error" } else { "ok" };
                let _ = store
                    .insert_consume_record(&task_id, &model, usage.input_tokens, usage.output_tokens, 0, status)
                    .await;
            }
        }
    })
    .await
}

/// Rewrite a request body's `model`, so the forward — and the metering row that
/// follows it — both name the model actually served. A body that is not a JSON
/// object is passed through untouched: there is nothing to relabel, and the
/// gateway's own parser is the right place for it to fail.
fn relabel_model(bytes: &[u8], model: &str) -> axum::body::Bytes {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return axum::body::Bytes::copy_from_slice(bytes);
    };
    let Some(obj) = v.as_object_mut() else {
        return axum::body::Bytes::copy_from_slice(bytes);
    };
    obj.insert("model".into(), serde_json::Value::String(model.to_string()));
    match serde_json::to_vec(&v) {
        Ok(out) => out.into(),
        Err(_) => axum::body::Bytes::copy_from_slice(bytes),
    }
}

fn extract_model_from_bytes(bytes: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_default()
}

/// Prefix on the gateway's provenance headers: which vendor actually served a
/// market request, and whether it was the buyer's own account.
///
/// Kept end to end rather than consumed here. The tool on the other side of
/// this proxy is the one that has to answer "did that go through Codex?", and
/// nothing else in the response can tell it — the model id is the one it asked
/// for and the text is in its own dialect.
const PROVENANCE_PREFIX: &str = "x-asale-";

/// Copy the gateway's provenance headers onto the response we hand the tool.
fn copy_provenance(from: &reqwest::header::HeaderMap, to: &mut axum::http::HeaderMap) {
    for (k, v) in from.iter() {
        if k.as_str().starts_with(PROVENANCE_PREFIX) {
            if let (Ok(name), Ok(value)) = (
                axum::http::HeaderName::from_bytes(k.as_str().as_bytes()),
                axum::http::HeaderValue::from_bytes(v.as_bytes()),
            ) {
                to.insert(name, value);
            }
        }
    }
}

/// One log line naming the upstream a market request landed on, so the answer
/// to "which subscription served this?" is in `~/.asale/asale.log` without
/// anyone having to read the SQLite records back.
fn log_provenance(headers: &reqwest::header::HeaderMap) {
    let get = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
    let upstream = get("x-asale-upstream");
    if upstream.is_empty() {
        return; // an older gateway, or not a market response
    }
    let own = get("x-asale-self") == "1";
    tracing::info!(
        upstream,
        source = get("x-asale-source"),
        model = get("x-asale-model"),
        task = get("x-asale-task"),
        own_account = own,
        "market request served{}",
        if own { " by this account's own lane — it spends your own quota" } else { "" }
    );
}

/// Pass a non-streamed upstream response straight through.
fn passthrough(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    // Taken before `bytes_stream()` consumes the response.
    let upstream_headers = resp.headers().clone();
    let mut out = Response::builder()
        .status(status)
        .header("content-type", ct)
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap();
    copy_provenance(&upstream_headers, out.headers_mut());
    out
}

/// Meter an upstream answer, whichever shape it arrived in, and hand it to the
/// caller.
///
/// Streaming and non-streaming answers report usage in completely different
/// places — SSE `data:` frames versus a `usage` object in a plain JSON body —
/// and [`UsageScanner`] only knows the first. Sending every response through it
/// meant a non-streaming call was metered as zero tokens on the buy side no
/// matter what it spent: the same blind spot as the missing `input_tokens`, on
/// the other half of the routes.
async fn meter_response<F, Fut>(resp: reqwest::Response, finish: F) -> Response
where
    F: FnOnce(Usage, bool) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let sse = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("event-stream"));
    if sse {
        stream_with_metering(resp, finish)
    } else {
        buffered_with_metering(resp, finish).await
    }
}

/// Meter a non-streamed answer: the whole body is read, its `usage` object is
/// what settles the call, and the bytes go on to the caller unchanged.
async fn buffered_with_metering<F, Fut>(resp: reqwest::Response, finish: F) -> Response
where
    F: FnOnce(Usage, bool) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let status = resp.status();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let upstream_headers = resp.headers().clone();

    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            // The answer is already lost; still settle, so the call is not
            // recorded as having served nothing at no cost.
            finish(Usage::default(), true).await;
            return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response();
        }
    };
    finish(asale_client_core::executor::usage_from_body(&body), false).await;

    let mut out = Response::builder().status(status).header("content-type", ct).body(Body::from(body)).unwrap();
    copy_provenance(&upstream_headers, out.headers_mut());
    out
}

/// Stream the upstream body through while accumulating usage from SSE frames;
/// invoke `finish(usage, had_error)` exactly once when the stream completes.
fn stream_with_metering<F, Fut>(resp: reqwest::Response, finish: F) -> Response
where
    F: FnOnce(Usage, bool) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let status = resp.status();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    // Taken before the response is moved into the pump task below.
    let upstream_headers = resp.headers().clone();

    let (tx, rx) = mpsc::unbounded_channel::<Result<axum::body::Bytes, std::io::Error>>();
    tokio::spawn(async move {
        // Same scanner the publisher side uses: a usage frame split across two
        // transport chunks would otherwise meter the call as zero tokens.
        let mut scan = UsageScanner::new();
        let mut had_error = false;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(b) => {
                    scan.push(&b);
                    if tx.send(Ok(b)).is_err() {
                        break; // client went away; still finish metering below
                    }
                }
                Err(e) => {
                    had_error = true;
                    let _ = tx.send(Err(std::io::Error::other(e)));
                    break;
                }
            }
        }
        finish(scan.flush(), had_error).await;
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    let mut out = Response::builder()
        .status(status)
        .header("content-type", ct)
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap();
    copy_provenance(&upstream_headers, out.headers_mut());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use asale_client_core::Strategy;

    /// A proxy state with every tool's buy switch on, so routing tests exercise
    /// routing rather than the buy gate (which has its own tests below).
    async fn state(key: Option<&str>) -> ProxyState {
        let store = LocalStore::open_memory().await.unwrap();
        for tool in crate::tool_config::TOOLS {
            store.set_buy_tool(tool, Some(true), None, None, None).await.unwrap();
        }
        ProxyState {
            server_api_base: "http://127.0.0.1:1".into(), // unreachable on purpose
            asale_key: Arc::new(tokio::sync::RwLock::new(key.map(String::from))),
            http: asale_client_core::http::plain(),
            store: Arc::new(store),
            pool: Arc::new(StdMutex::new(AccountPool::new(Strategy::RoundRobin))),
            app: None,
            remint_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn post_message(port: u16, model: &str) -> reqwest::Response {
        asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .json(&serde_json::json!({"model": model, "messages": []}))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn buy_switch_off_refuses_instead_of_spending() {
        // A tool whose switch is off has had its config restored, so this only
        // happens for a stale process still pointed at us — refuse, don't bill.
        let st = state(Some("sk-asale-test")).await;
        st.store.set_buy_tool("claude", Some(false), None, None, None).await.unwrap();
        let store = st.store.clone();
        let port = serve(st).await;

        let resp = post_message(port, "claude-sonnet-4-5").await;
        assert_eq!(resp.status(), 403, "buying off for claude -> refused");
        let (_, total) = store.list_consume_records(10, 0).await.unwrap();
        assert_eq!(total, 0, "a refused request is never metered");
    }

    #[tokio::test]
    async fn a_selected_model_passes_the_gate_untouched() {
        let st = state(Some("sk-asale-test")).await;
        // Claude Code may only buy haiku; Gemini's list stays empty (= any).
        st.store
            .set_buy_tool("claude", None, Some(&["claude-3-5-haiku-latest".into()]), None, None)
            .await
            .unwrap();
        let port = serve(st).await;

        // A model that *is* selected passes the gate and reaches routing (502:
        // the market target is unreachable in tests).
        let resp = post_message(port, "claude-3-5-haiku-latest").await;
        assert_eq!(resp.status(), 502, "selected model passes the gate");
    }

    #[tokio::test]
    async fn a_model_the_tool_cannot_be_asked_for_is_relabelled_not_refused() {
        // Claude Code's picker only lists Anthropic ids, so buying gpt-5.1 for
        // it means *every* call — the ones the user typed and the background
        // ones alike — names a model outside the list. They must reach the
        // market as the model that was bought; the gateway does the dialect
        // translation from there.
        let (addr, rx) = capturing_gateway().await;
        let mut st = state(Some("sk-asale-test")).await;
        st.server_api_base = format!("http://{addr}");
        st.store.set_buy_tool("claude", None, Some(&["gpt-5.1".into()]), None, None).await.unwrap();
        let port = serve(st).await;

        let resp = post_message(port, "claude-opus-5").await;
        assert_eq!(resp.status(), 200, "served, not 403'd");
        assert_eq!(captured_body(rx.await.unwrap())["model"], "gpt-5.1", "relabelled to the bought model");
    }

    #[test]
    fn strip_release_stamp_removes_stamps_and_tags_only() {
        assert_eq!(strip_release_stamp("claude-fable-5-e2e-1784891711622250000"), "claude-fable-5");
        assert_eq!(strip_release_stamp("claude-opus-4-8-20260101"), "claude-opus-4-8");
        assert_eq!(strip_release_stamp("claude-3-5-haiku-latest"), "claude-3-5-haiku");
        assert_eq!(strip_release_stamp("claude-3-5-haiku"), "claude-3-5-haiku");
        assert_eq!(strip_release_stamp("gpt-5-codex"), "gpt-5-codex");
        assert_eq!(strip_release_stamp("gemini-2.5-pro"), "gemini-2.5-pro");
    }

    /// The buy switch stores families, the market sells ids: a traded model
    /// whose id got shortened here would never match the user's selection and
    /// its traffic would be refused. Every id the platform prices
    /// (`asale-server/src/catalog.rs`) must therefore survive unchanged.
    #[test]
    fn traded_model_ids_are_their_own_family() {
        for id in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-haiku-4-5",
            "gpt-5-codex",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
        ] {
            assert_eq!(strip_release_stamp(id), id, "`{id}` must not be shortened");
        }
    }

    #[tokio::test]
    async fn a_family_selection_admits_its_release_ids() {
        let st = state(Some("sk-asale-test")).await;
        // What the UI now stores: the family name, no release stamp.
        st.store.set_buy_tool("claude", None, Some(&["claude-fable-5".into()]), None, None).await.unwrap();
        let port = serve(st).await;

        let resp = post_message(port, "claude-fable-5-e2e-1784891711622250000").await;
        assert_eq!(resp.status(), 502, "a release id of the selected family passes the gate");
    }

    #[tokio::test]
    async fn another_family_is_relabelled_to_the_selection() {
        // The flip side of the test above: a stamped id from a family the user
        // did *not* select must not be mistaken for the selected one — it is
        // served as the selected model instead of passing through untouched.
        let (addr, rx) = capturing_gateway().await;
        let mut st = state(Some("sk-asale-test")).await;
        st.server_api_base = format!("http://{addr}");
        st.store.set_buy_tool("claude", None, Some(&["claude-fable-5".into()]), None, None).await.unwrap();
        let port = serve(st).await;

        let resp = post_message(port, "claude-opus-4-8-20260101").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(captured_body(rx.await.unwrap())["model"], "claude-fable-5", "served as the bought family");
    }

    #[tokio::test]
    async fn buy_list_of_one_tool_does_not_restrict_another() {
        let st = state(Some("sk-asale-test")).await;
        st.store.set_buy_tool("claude", None, Some(&["claude-3-5-haiku-latest".into()]), None, None).await.unwrap();
        let port = serve(st).await;
        // /v1/chat/completions belongs to codex, whose list is empty = any model.
        let resp = asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .json(&serde_json::json!({"model": "gpt-5-codex", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 502, "codex is gated by codex's own list, not claude's");
    }

    #[tokio::test]
    async fn responses_endpoint_is_codex_traffic() {
        // Codex ≥ 0.146 only speaks /v1/responses; it must land on codex's own
        // buy switch and model list, not on some other tool's.
        let st = state(Some("sk-asale-test")).await;
        st.store.set_buy_tool("codex", None, Some(&["claude-fable-5".into()]), None, None).await.unwrap();
        let port = serve(st).await;
        let post = |model: &str| {
            let body = serde_json::json!({"model": model, "input": "hi", "stream": true});
            asale_client_core::http::plain().post(format!("http://127.0.0.1:{port}/v1/responses")).json(&body).send()
        };

        let resp = post("gpt-5.2").await.unwrap();
        assert_eq!(resp.status(), 502, "codex's own catalog models are served, not refused");
        let resp = post("claude-fable-5").await.unwrap();
        assert_eq!(resp.status(), 502, "the selected model reaches routing (market unreachable in tests)");
    }

    /// A one-shot stand-in for the asale gateway that hands back the request
    /// body it was sent, so a test can assert on what actually left the proxy.
    async fn capturing_gateway() -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<Vec<u8>>) {
        let (tx, rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            if let Ok((mut sock, _)) = listener.accept().await {
                // Read until the whole body has arrived: headers and body do
                // not reliably land in one TCP segment.
                let mut raw = Vec::new();
                loop {
                    let mut chunk = [0u8; 2048];
                    match sock.read(&mut chunk).await {
                        Ok(0) => break,
                        Ok(n) => raw.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
                    let want: usize = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    if body.len() >= want {
                        break;
                    }
                }
                let _ = tx.send(raw);
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                    .await;
            }
        });
        (addr, rx)
    }

    /// The JSON body `capturing_gateway` received.
    fn captured_body(raw: Vec<u8>) -> serde_json::Value {
        let raw = String::from_utf8_lossy(&raw).into_owned();
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap_or_default()).unwrap()
    }

    #[tokio::test]
    async fn codex_internal_models_are_relabelled_not_refused() {
        // The ChatGPT app titles threads and runs auto-review against ids from
        // its built-in catalog, which the user never picked and cannot change.
        // Those requests must reach the market as the model that *was* bought.
        let (addr, rx) = capturing_gateway().await;
        let mut st = state(Some("sk-asale-test")).await;
        st.server_api_base = format!("http://{addr}");
        st.store.set_buy_tool("codex", None, Some(&["claude-fable-5".into(), "claude-haiku-4-5".into()]), None, None).await.unwrap();
        let port = serve(st).await;

        let resp = asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}/v1/responses"))
            .json(&serde_json::json!({"model": "gpt-5.6-luna", "input": "title this"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "an internal codex model is served, not 403'd");

        let sent = captured_body(rx.await.unwrap());
        assert_eq!(sent["model"], "claude-fable-5", "relabelled to the first bought model");
        assert_eq!(sent["input"], "title this", "the rest of the request survives");
    }

    #[tokio::test]
    async fn codex_picker_slugs_are_translated_to_the_model_they_stand_for() {
        // The picker offers market models under Codex's own slugs, because the
        // desktop app drops anything else (see `codex_catalog`). Picking the
        // second entry must buy the second model — not the first, which is what
        // the plain "internal model" relabel would have done.
        let _g = crate::codex_catalog::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var("HOME").ok();
        let home = std::env::temp_dir().join(format!("asale-proxy-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".asale")).unwrap();
        std::env::set_var("HOME", &home);
        std::fs::write(
            crate::codex_catalog::aliases_path(),
            r#"{"gpt-5.5":"claude-fable-5","gpt-5.2":"claude-haiku-4-5"}"#,
        )
        .unwrap();

        let (addr, rx) = capturing_gateway().await;
        let mut st = state(Some("sk-asale-test")).await;
        st.server_api_base = format!("http://{addr}");
        st.store.set_buy_tool("codex", None, Some(&["claude-fable-5".into(), "claude-haiku-4-5".into()]), None, None).await.unwrap();
        let port = serve(st).await;

        let resp = asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}/v1/responses"))
            .json(&serde_json::json!({"model": "gpt-5.2", "input": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(captured_body(rx.await.unwrap())["model"], "claude-haiku-4-5", "the carrier's own model");

        match prev_home {
            Some(p) => std::env::set_var("HOME", p),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn a_stale_alias_cannot_smuggle_a_dropped_model_past_the_gate() {
        // The alias table on disk outlives a selection change by however long
        // it takes to rewrite it. A slug still pointing at a model the user has
        // since deselected must fall through to the ordinary gate.
        let _g = crate::codex_catalog::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var("HOME").ok();
        let home = std::env::temp_dir().join(format!("asale-proxy-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".asale")).unwrap();
        std::env::set_var("HOME", &home);
        std::fs::write(crate::codex_catalog::aliases_path(), r#"{"gpt-5.5":"claude-opus-5"}"#).unwrap();

        let (addr, rx) = capturing_gateway().await;
        let mut st = state(Some("sk-asale-test")).await;
        st.server_api_base = format!("http://{addr}");
        st.store.set_buy_tool("codex", None, Some(&["claude-fable-5".into()]), None, None).await.unwrap();
        let port = serve(st).await;

        let resp = asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}/v1/responses"))
            .json(&serde_json::json!({"model": "gpt-5.5", "input": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "codex traffic is still served rather than 403'd");
        assert_eq!(
            captured_body(rx.await.unwrap())["model"],
            "claude-fable-5",
            "served as a model the user actually bought, not the stale alias"
        );

        match prev_home {
            Some(p) => std::env::set_var("HOME", p),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn responses_switch_off_refuses() {
        let st = state(Some("sk-asale-test")).await;
        st.store.set_buy_tool("codex", Some(false), None, None, None).await.unwrap();
        let port = serve(st).await;
        let resp = asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}/v1/responses"))
            .json(&serde_json::json!({"model": "claude-fable-5", "input": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
    }

    /// POST to an arbitrary path, for the `/{tool}` addressing forms.
    async fn post_to(port: u16, path: &str, model: &str) -> reqwest::Response {
        asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}{path}"))
            .json(&serde_json::json!({"model": model, "messages": []}))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_prefixed_request_is_gated_by_its_own_tools_switch() {
        // OpenClaw speaks the same wire format as Codex, so without the prefix
        // this request would be gated on Codex's switch and handed Codex's
        // model list. The prefix is the only thing that distinguishes them.
        let st = state(Some("sk-x")).await;
        st.store.set_buy_tool("openclaw", Some(false), None, None, None).await.unwrap();
        let port = serve(st).await;

        let resp = post_to(port, "/openclaw/v1/chat/completions", "claude-fable-5").await;
        assert_eq!(resp.status(), 403, "openclaw is off, and it is openclaw's switch that decides");
        assert!(resp.text().await.unwrap().contains("openclaw"), "the refusal names the tool that is off");

        // Codex, still on, is unaffected by its neighbour's switch.
        let resp = post_to(port, "/v1/chat/completions", "claude-fable-5").await;
        assert_eq!(resp.status(), 502, "codex passes its own gate and reaches routing");
    }

    #[tokio::test]
    async fn a_prefix_naming_no_known_tool_is_refused_rather_than_forwarded() {
        // These routes match any leading segment. An unrecognized one has no
        // switch behind it, so serving it would spend the balance ungated.
        let port = serve(state(Some("sk-x")).await).await;
        let resp = post_to(port, "/not-a-tool/v1/messages", "claude-fable-5").await;
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn the_prefix_is_not_forwarded_upstream() {
        let (addr, rx) = capturing_gateway().await;
        let mut st = state(Some("sk-x")).await;
        st.server_api_base = format!("http://{addr}");
        st.store.set_buy_tool("openclaw", Some(true), Some(&[]), None, None).await.unwrap();
        let port = serve(st).await;

        let resp = post_to(port, "/openclaw/v1/chat/completions", "claude-fable-5").await;
        assert_eq!(resp.status(), 200);
        let raw = rx.await.unwrap();
        let request_line = String::from_utf8_lossy(&raw).lines().next().unwrap_or_default().to_string();
        assert!(
            request_line.starts_with("POST /v1/chat/completions"),
            "the gateway serves the dialect path, not our addressing prefix — got {request_line:?}"
        );
    }

    async fn serve(st: ProxyState) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router(st)).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        port
    }

    #[tokio::test]
    async fn rejects_without_api_key() {
        let st = state(None).await;
        let port = serve(st).await;
        let resp = asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .json(&serde_json::json!({"model": "x", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "no api key configured -> 401");
    }

    #[tokio::test]
    async fn healthz_ok() {
        let st = state(Some("sk-asale-test")).await;
        let port = serve(st).await;
        let resp =
            asale_client_core::http::plain().get(format!("http://127.0.0.1:{port}/healthz")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn market_failure_writes_consume_record() {
        // The market target is unreachable → 502, but a failed forward with a
        // reachable-but-erroring server records an upstream_<code> row. Here we
        // exercise the unreachable branch: no row (send() failed before any
        // upstream status). Then simulate an error status via a local stub.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 402 Payment Required\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                    .await;
            }
        });
        let mut st = state(Some("sk-asale-test")).await;
        st.server_api_base = format!("http://{addr}");
        let store = st.store.clone();
        let port = serve(st).await;

        let resp = asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .json(&serde_json::json!({"model": "claude-sonnet-4-5", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 402, "server error status passes through");
        let (rows, total) = store.list_consume_records(10, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].status, "upstream_402");
        assert_eq!(rows[0].model, "claude-sonnet-4-5");
    }

    #[tokio::test]
    async fn count_tokens_is_forwarded_but_not_metered() {
        // A stub gateway that answers count_tokens with a normal 200 JSON body.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = b"{\"input_tokens\":42}";
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
            }
        });
        let mut st = state(Some("sk-asale-test")).await;
        st.server_api_base = format!("http://{addr}");
        let store = st.store.clone();
        let port = serve(st).await;

        let resp = asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}/v1/messages/count_tokens"))
            .json(&serde_json::json!({"model": "claude-sonnet-4-5", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "count_tokens forwarded to the gateway");
        // No consume record: count_tokens is not a billable completion.
        let (_, total) = store.list_consume_records(10, 0).await.unwrap();
        assert_eq!(total, 0, "count_tokens must not be metered");
    }

    #[tokio::test]
    async fn direct_mode_without_accounts_says_so() {
        let st = state(Some("sk-asale-test")).await;
        st.store.set_setting("consume_mode", "direct").await.unwrap();
        let port = serve(st).await;
        let resp = asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .json(&serde_json::json!({"model": "claude-sonnet-4-5"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 503, "direct with no local account -> explicit 503");
    }

    #[tokio::test]
    async fn auto_mode_falls_back_to_market_without_accounts() {
        // auto + empty pool → market → 401 because no asale key is configured.
        let st = state(None).await;
        st.store.set_setting("consume_mode", "auto").await.unwrap();
        let port = serve(st).await;
        let resp = asale_client_core::http::plain()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .json(&serde_json::json!({"model": "claude-sonnet-4-5"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "auto fell back to the market route");
    }
}
