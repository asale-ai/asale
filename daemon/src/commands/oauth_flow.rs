//! Browser OAuth, both flavours, as a two-step flow: begin returns
//! `{flow_id, auth_url}`, the frontend opens it, then polls `oauth_result`
//! until the loopback callback and token exchange finish.

use crate::keychain;
use crate::oauth;
use crate::publisher;
use crate::state::{AppState, FlowStatus};
use serde_json::{json, Value};
use std::sync::Arc;
use super::sell::{accounts_changed};
use super::server_client::{finish_auth, resp_json};
use super::{R, err, now_secs};

/// Record a flow result.
pub(crate) async fn flow_set(state: &AppState, id: &str, status: FlowStatus) {
    state.oauth_flows.write().await.insert(id.to_string(), status);
}

/// Poll an in-flight OAuth flow. Terminal results are consumed (removed).
pub async fn oauth_result(state: &AppState, flow_id: String) -> R<Value> {
    let mut flows = state.oauth_flows.write().await;
    match flows.get(&flow_id) {
        None => Err("unknown flow".to_string()),
        Some(FlowStatus::Pending) => Ok(json!({ "status": "pending" })),
        Some(FlowStatus::Done(v)) => {
            let v = v.clone();
            flows.remove(&flow_id);
            Ok(json!({ "status": "ok", "result": v }))
        }
        Some(FlowStatus::Failed(e)) => {
            let e = e.clone();
            flows.remove(&flow_id);
            Ok(json!({ "status": "error", "error": e }))
        }
    }
}

/// Open `url` in the machine-local browser, best-effort — only meaningful when
/// the daemon runs on a machine with a desktop (the Tauri shell case, or a
/// local browser session). Headless boxes simply skip.
pub(crate) fn open_local_browser(url: &str) {
    #[cfg(target_os = "linux")]
    {
        // On headless Linux there is no display server — don't block on xdg-open.
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return;
        }
    }
    let _ = open::that_detached(url);
}

/// Begin an OAuth login for a provider. Starts the loopback callback listener,
/// returns `{flow_id, auth_url}` right away, and finishes the exchange in the
/// background; the frontend polls `oauth_result`. `open_local=true` also opens
/// the URL in the daemon machine's browser (used by the desktop shell).
///
/// Note (remote B/S): the callback listens on this machine's loopback. When the
/// UI runs in a browser on a *different* machine, complete the authorization
/// through an SSH port-forward, or use `import_from_cli` instead.
pub async fn oauth_login(state: &Arc<AppState>, provider: String, open_local: bool) -> R<Value> {
    let p = oauth::provider(&provider).ok_or("unknown provider")?;
    let (url, fut) = oauth::begin(&p).await.map_err(err)?;
    let flow_id = uuid::Uuid::new_v4().simple().to_string();
    flow_set(state, &flow_id, FlowStatus::Pending).await;

    if open_local {
        open_local_browser(&url);
    }

    let st = state.clone();
    let fid = flow_id.clone();
    let prov = provider.clone();
    tokio::spawn(async move {
        let outcome = finish_provider_oauth(&st, &p, &prov, fut).await;
        match outcome {
            Ok(v) => flow_set(&st, &fid, FlowStatus::Done(v)).await,
            Err(e) => flow_set(&st, &fid, FlowStatus::Failed(e)).await,
        }
    });

    Ok(json!({ "flow_id": flow_id, "auth_url": url, "provider": provider }))
}

/// Wait for the provider callback, exchange the code, persist tokens + tool row.
pub(crate) async fn finish_provider_oauth(
    state: &AppState,
    p: &oauth::OAuthProvider,
    provider: &str,
    fut: oauth::AuthCodeFuture,
) -> R<Value> {
    let code = tokio::time::timeout(std::time::Duration::from_secs(300), fut.wait())
        .await
        .map_err(|_| "authorization timed out".to_string())?
        .map_err(err)?;
    let tokens = oauth::exchange(p, &code).await.map_err(err)?;
    let access = tokens["access_token"].as_str().unwrap_or_default();
    if access.is_empty() {
        return Err(format!("token exchange failed: {tokens}"));
    }
    let account_id = tokens["account"]["email"]
        .as_str()
        .or_else(|| tokens["email"].as_str())
        .unwrap_or("account")
        .to_string();
    let plan = tokens["account"]["plan"].as_str().or_else(|| tokens["plan"].as_str());

    // Persist tokens in the secret store; the store only holds references (§3.4).
    keychain::set(&keychain::token_ref(provider, &account_id), access).map_err(err)?;
    if let Some(refresh) = tokens["refresh_token"].as_str() {
        keychain::set(&keychain::refresh_ref(provider, &account_id), refresh).map_err(err)?;
    }
    if let Some(secs) = tokens["expires_in"].as_i64() {
        let exp = (now_secs() + secs).to_string();
        state.store.set_setting(&publisher::exp_key(provider, &account_id), &exp).await.map_err(err)?;
    }
    // origin=oauth: asale ran this login itself, so it exclusively owns the
    // resulting refresh token — no rotation conflict with a local CLI (§4).
    state
        .store
        .upsert_tool(provider, &account_id, &keychain::token_ref(provider, &account_id), &["oauth"], "oauth")
        .await
        .map_err(err)?;
    if let Some(plan) = plan {
        let _ = state.store.set_setting(&format!("plan:{provider}:{account_id}"), plan).await;
    }
    accounts_changed(state).await;

    Ok(json!({"provider": provider, "account_id": account_id, "keychain_ref": keychain::token_ref(provider, &account_id)}))
}

/// Sign in (or link) via a platform OAuth provider (google | github). PKCE +
/// loopback callback; the server builds the authorize URL and exchanges the
/// code. `link=false` logs in and stores tokens; `link=true` binds the
/// identity to the currently signed-in account. Two-step like `oauth_login`.
pub async fn platform_oauth_login(
    state: &Arc<AppState>,
    provider: String,
    link: bool,
    open_local: bool,
    region: String,
) -> R<Value> {
    if provider != "google" && provider != "github" {
        return Err("unknown provider".to_string());
    }
    // Resolve the link token up front so we fail before opening the browser.
    let link_token = if link {
        Some(keychain::get("access_token").map_err(err)?.ok_or("not logged in")?)
    } else {
        None
    };

    let pkce = oauth::gen_pkce();
    let cb = oauth::begin_platform_loopback().await.map_err(err)?;
    let redirect_uri = cb.redirect_uri.clone();
    let csrf = uuid::Uuid::new_v4().to_string();

    let http = asale_client_core::http::plain();
    let resp = http
        .get(format!("{}/api/v1/auth/oauth/{}/authorize", state.cfg.server_api_base, provider))
        .query(&[
            ("redirect_uri", redirect_uri.as_str()),
            ("state", csrf.as_str()),
            ("code_challenge", pkce.challenge.as_str()),
            // Picks the provider registration that accepts an ephemeral
            // loopback callback; the web app's cannot. Must be repeated on the
            // exchange, or the code goes to a different client_id.
            ("client", "desktop"),
        ])
        .send()
        .await
        .map_err(err)?;
    let v = resp_json(resp).await?;
    let auth_url = v["auth_url"].as_str().ok_or("server returned no auth_url")?.to_string();

    let flow_id = uuid::Uuid::new_v4().simple().to_string();
    flow_set(state, &flow_id, FlowStatus::Pending).await;
    if open_local {
        open_local_browser(&auth_url);
    }

    let st = state.clone();
    let fid = flow_id.clone();
    let prov = provider.clone();
    tokio::spawn(async move {
        let out: R<Value> = async {
            let code = cb.wait(&csrf, std::time::Duration::from_secs(300)).await.map_err(err)?;
            let http = asale_client_core::http::plain();
            let mut req = http
                .post(format!("{}/api/v1/auth/oauth/{}/exchange", st.cfg.server_api_base, prov))
                // `region` is the country the sign-up screen collected; the
                // server applies it only if this exchange creates an account,
                // so sending it on a returning user's login is a no-op.
                .json(&json!({
                    "code": code,
                    "redirect_uri": redirect_uri,
                    "code_verifier": pkce.verifier,
                    "region": region,
                    "client": "desktop",
                }));
            if let Some(token) = link_token {
                req = req.header("authorization", format!("Bearer {token}"));
            }
            let v = resp_json(req.send().await.map_err(err)?).await?;
            if link {
                Ok(json!({"linked": v["linked"], "provider": v["provider"], "email": v["email"]}))
            } else {
                finish_auth(&v).await
            }
        }
        .await;
        match out {
            Ok(v) => flow_set(&st, &fid, FlowStatus::Done(v)).await,
            Err(e) => flow_set(&st, &fid, FlowStatus::Failed(e)).await,
        }
    });

    Ok(json!({ "flow_id": flow_id, "auth_url": auth_url, "provider": provider }))
}
