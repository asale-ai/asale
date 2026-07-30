//! Browser OAuth, both flavours, as a two-step flow: begin returns
//! `{flow_id, auth_url}`, the frontend opens it, then polls `oauth_result`
//! until the loopback callback and token exchange finish.

use crate::keychain;
use crate::oauth;
use crate::publisher;
use crate::state::{AppState, FlowStatus};
use asale_client_core::{cli_import, device_flow, Provider};
use serde_json::{json, Value};
use std::sync::Arc;
use super::sell::{accounts_changed};
use super::server_client::{finish_auth, resp_json};
use super::{CmdError, R, err, err_keyed, now_secs};
use crate::cmd_err;

/// Record a flow result.
pub(crate) async fn flow_set(state: &AppState, id: &str, status: FlowStatus) {
    state.oauth_flows.write().await.insert(id.to_string(), status);
}

/// Poll an in-flight OAuth flow. Terminal results are consumed (removed).
pub async fn oauth_result(state: &AppState, flow_id: String) -> R<Value> {
    let mut flows = state.oauth_flows.write().await;
    match flows.get(&flow_id) {
        None => Err(cmd_err!("errors.oauth.unknownFlow", "unknown flow")),
        Some(FlowStatus::Pending) => Ok(json!({ "status": "pending" })),
        Some(FlowStatus::Done(v)) => {
            let v = v.clone();
            flows.remove(&flow_id);
            Ok(json!({ "status": "ok", "result": v }))
        }
        Some(FlowStatus::Failed(e)) => {
            // Shaped like the RPC failure envelope (`error` + `key` + `params`)
            // so the frontend translates a flow that failed asynchronously the
            // same way it translates one that failed inline.
            let mut out = e.to_json();
            out["status"] = json!("error");
            flows.remove(&flow_id);
            Ok(out)
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
        // Provider token responses may still contain a refresh token or an ID
        // token when they are malformed. Never echo the response into the RPC
        // error: that error is rendered by the frontend and may be collected by
        // browser diagnostics.
        return Err(missing_access_token_error(provider, &tokens));
    }
    let account_id = tokens["account"]["email"]
        .as_str()
        .or_else(|| tokens["email"].as_str())
        .map(str::to_string)
        // Codex returns neither — the identity lives in the id_token claims.
        .or_else(|| oauth::id_token_claim(&tokens, "email"))
        .unwrap_or_else(|| "account".to_string());
    // Codex returns the plan in neither field — like the email above, it lives in
    // the id_token claims, in the same `https://api.openai.com/auth` block this
    // function already reads the ChatGPT account id out of. Missing it left every
    // asale-logged Codex account at `plan: None`, which
    // `discovery::plan_window_cap` reads as the lowest tier (200k tokens per 5h
    // window) no matter what the subscription actually allows — so the lanes went
    // dark after a handful of full-size sales and matching answered `no_supply`.
    let claim_plan = oauth::id_token_claim(&tokens, "chatgpt_plan_type").or_else(|| {
        tokens["id_token"]
            .as_str()
            .and_then(cli_import::jwt_claims)
            .as_ref()
            .and_then(|c| c.get("https://api.openai.com/auth"))
            .and_then(|a| a.get("chatgpt_plan_type"))
            .and_then(|p| p.as_str())
            .map(str::to_string)
    });
    let plan = tokens["account"]["plan"]
        .as_str()
        .or_else(|| tokens["plan"].as_str())
        .map(str::to_string)
        .or(claim_plan)
        .filter(|p| !p.is_empty());

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
    if let Some(plan) = &plan {
        let _ = state.store.set_setting(&format!("plan:{provider}:{account_id}"), plan).await;
    }
    // Codex's upstream will not accept this bearer without the ChatGPT account
    // id issued with it, and asale's own login is the only place that id passes
    // through — the CLI import reads it out of auth.json, this path has to read
    // it out of the exchange. Missing it means every sale 401s.
    if let Some(up) = tokens["id_token"]
        .as_str()
        .and_then(cli_import::jwt_claims)
        .as_ref()
        .and_then(|c| c.get("https://api.openai.com/auth"))
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|a| a.as_str())
        .filter(|a| !a.is_empty())
    {
        let _ = state
            .store
            .set_setting(&publisher::upstream_acct_key(provider, &account_id), up)
            .await;
    }
    accounts_changed(state).await;

    Ok(json!({"provider": provider, "account_id": account_id, "keychain_ref": keychain::token_ref(provider, &account_id)}))
}

fn missing_access_token_error(provider: &str, tokens: &Value) -> CmdError {
    let provider_error = tokens["error"]
        .as_str()
        .or_else(|| tokens["error"]["code"].as_str())
        .unwrap_or("missing_access_token");
    cmd_err!(
        "errors.oauth.exchangeFailed",
        format!("token exchange failed ({provider_error})"),
        provider = provider,
        reason = provider_error
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_token_response_never_reaches_the_rpc_error() {
        let response = json!({
            "error": {"code": "invalid_grant"},
            "refresh_token": "refresh-secret",
            "id_token": "identity-secret",
            "client_secret": "client-secret"
        });
        let encoded = missing_access_token_error("codex", &response).to_json().to_string();
        assert!(encoded.contains("invalid_grant"));
        for secret in ["refresh-secret", "identity-secret", "client-secret"] {
            assert!(!encoded.contains(secret));
        }
    }
}

/// Begin a device-code login (Kimi Code, Grok CLI).
///
/// Same two-step contract as `oauth_login` — returns a `flow_id` the frontend
/// polls — but there is no loopback listener and therefore no requirement that
/// the browser run on this machine. That is what makes these two connectable
/// from a remote B/S session, where the authorization-code providers are not.
///
/// The extra fields are what the user needs on screen: `user_code` is the short
/// string they confirm, `auth_url` is where they confirm it (pre-filled with
/// the code when the provider offers that form).
pub async fn oauth_device_login(state: &Arc<AppState>, provider: String, open_local: bool) -> R<Value> {
    let p = Provider::from_str_opt(&provider)
        .filter(|p| asale_protocol::ids::is_device_flow_provider(*p))
        .ok_or_else(|| {
            cmd_err!(
                "errors.deviceFlow.unsupportedProvider",
                "this provider does not use a device-code login (kimi | xai)"
            )
        })?;

    let code = device_flow::begin(p).await.map_err(err_keyed)?;
    let flow_id = uuid::Uuid::new_v4().simple().to_string();
    flow_set(state, &flow_id, FlowStatus::Pending).await;

    // Read out what the user has to see before the polling task takes the code.
    let auth_url = code.verification_uri.clone();
    let user_code = code.user_code.clone();
    let expires_in = code.expires_in;

    if open_local {
        open_local_browser(&auth_url);
    }

    let st = state.clone();
    let fid = flow_id.clone();
    let prov = provider.clone();
    tokio::spawn(async move {
        let outcome = finish_device_login(&st, p, &prov, code).await;
        match outcome {
            Ok(v) => flow_set(&st, &fid, FlowStatus::Done(v)).await,
            Err(e) => flow_set(&st, &fid, FlowStatus::Failed(e)).await,
        }
    });

    Ok(json!({
        "flow_id": flow_id,
        "provider": provider,
        // `auth_url` keeps the same name the authorization-code flow uses, so
        // the frontend's existing open-and-poll helper drives both unchanged.
        "auth_url": auth_url,
        "user_code": user_code,
        "expires_in": expires_in,
    }))
}

/// Poll the token endpoint until the user approves, then persist exactly like
/// an authorization-code login: the credential is asale's own, so `origin` is
/// `oauth` and no shared-rotation warning applies.
async fn finish_device_login(
    state: &AppState,
    p: Provider,
    provider: &str,
    code: device_flow::DeviceCode,
) -> R<Value> {
    let tokens = device_flow::poll(p, &code).await.map_err(err_keyed)?;

    // An OIDC id_token names the account; Kimi issues none, so those accounts
    // fall back to a digest of the refresh token — stable across refreshes and
    // distinct per login, the same rule the CLI import uses.
    let account_id = tokens
        .id_token
        .as_deref()
        .and_then(cli_import::jwt_claims)
        .and_then(|c| {
            c.get("email")
                .or_else(|| c.get("sub"))
                .and_then(|e| e.as_str())
                .map(String::from)
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let cred = cli_import::CliCred {
                provider: provider.to_string(),
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                expires_at: tokens.expires_at,
                account_hint: None,
                plan: None,
                // Device-flow providers (Kimi) carry no vendor-side account id.
                upstream_account_id: None,
            };
            format!("{provider}-{}", cli_import::token_fingerprint(&cred))
        });

    keychain::set(&keychain::token_ref(provider, &account_id), &tokens.access_token).map_err(err)?;
    if let Some(refresh) = &tokens.refresh_token {
        keychain::set(&keychain::refresh_ref(provider, &account_id), refresh).map_err(err)?;
    }
    if let Some(exp) = tokens.expires_at {
        state
            .store
            .set_setting(&publisher::exp_key(provider, &account_id), &exp.to_string())
            .await
            .map_err(err)?;
    }
    state
        .store
        .upsert_tool(provider, &account_id, &keychain::token_ref(provider, &account_id), &["oauth"], "oauth")
        .await
        .map_err(err)?;
    accounts_changed(state).await;

    Ok(json!({"provider": provider, "account_id": account_id}))
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
        return Err(cmd_err!("errors.oauth.unknownProvider", "unknown provider", provider = provider.as_str()));
    }
    // Resolve the link token up front so we fail before opening the browser.
    let link_token = if link {
        Some(keychain::get("access_token").map_err(err)?.ok_or_else(|| cmd_err!("errors.session.notSignedIn", "not logged in"))?)
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
