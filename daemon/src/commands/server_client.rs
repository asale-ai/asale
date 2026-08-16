//! REST client for the asale server: token refresh, the authenticated
//! request helper every other command goes through, and response decoding.

use crate::keychain;
use crate::state::AppState;
use serde_json::{json, Value};
use super::{CmdError, R, err};
use crate::cmd_err;

/// Persist the token pair a sign-in returned, and hand the UI the account.
///
/// Takes the state only to forget what it cached about the *previous* account:
/// see [`super::forget_account_cache`] for why an account switch that skipped
/// this would delete somebody's custom endpoints.
pub(crate) async fn finish_auth(state: &AppState, v: &Value) -> R<Value> {
    let access = v["tokens"]["access_token"].as_str().unwrap_or_default();
    let refresh = v["tokens"]["refresh_token"].as_str().unwrap_or_default();
    if access.is_empty() {
        return Err(server_error(v, "auth failed"));
    }
    keychain::set("access_token", access).map_err(err)?;
    keychain::set("refresh_token", refresh).map_err(err)?;
    super::forget_account_cache(state).await;
    Ok(json!({
        "user_id": v["user_id"],
        "email": v["email"],
        "name": v["name"],
        "avatar_url": v["avatar_url"],
    }))
}

/// Refresh the token pair using the stored refresh token; returns the new
/// access token (and persists both in the secret store).
pub(crate) async fn refresh_tokens(state: &AppState, http: &reqwest::Client) -> R<String> {
    refresh_access_token(&state.cfg.server_api_base, http).await
}

/// Same refresh, against an explicit API base. The publisher's `ConfigSource`
/// runs outside the command layer and holds no [`AppState`], so it needs this
/// entry point to share the one refresh path instead of retrying a stale token
/// forever.
pub(crate) async fn refresh_access_token(api_base: &str, http: &reqwest::Client) -> R<String> {
    let refresh = keychain::get("refresh_token").map_err(err)?.ok_or_else(|| cmd_err!("errors.session.notSignedIn", "not logged in"))?;
    let resp = http
        .post(format!("{api_base}/api/v1/auth/refresh"))
        .json(&json!({"refresh_token": refresh}))
        .send()
        .await
        .map_err(err)?;
    let v = resp_json(resp).await.map_err(|_| cmd_err!("errors.session.expired", "session expired, sign in again"))?;
    let access = v["tokens"]["access_token"].as_str().unwrap_or_default().to_string();
    if access.is_empty() {
        return Err(cmd_err!("errors.session.expired", "session expired, sign in again"));
    }
    keychain::set("access_token", &access).map_err(err)?;
    if let Some(r) = v["tokens"]["refresh_token"].as_str() {
        keychain::set("refresh_token", r).map_err(err)?;
    }
    Ok(access)
}

/// Bearer-authenticated request to the server; on 401 refreshes once and retries.
pub(crate) async fn authed(state: &AppState, method: reqwest::Method, path: &str, body: Option<Value>) -> R<Value> {
    let http = asale_client_core::http::plain();
    let mut token = keychain::get("access_token").map_err(err)?.ok_or_else(|| cmd_err!("errors.session.notSignedIn", "not logged in"))?;
    for attempt in 0..2 {
        let mut req = http
            .request(method.clone(), format!("{}{}", state.cfg.server_api_base, path))
            .header("authorization", format!("Bearer {token}"));
        if let Some(b) = &body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(err)?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
            token = refresh_tokens(state, &http).await?;
            continue;
        }
        return resp_json(resp).await;
    }
    unreachable!("authed loop always returns")
}

pub(crate) async fn resp_json(resp: reqwest::Response) -> R<Value> {
    let status = resp.status();
    let v: Value = resp.json().await.map_err(err)?;
    if !status.is_success() {
        return Err(server_error(&v, "request failed"));
    }
    Ok(v)
}

/// Carry the server's failure through to the frontend *with its translation
/// key intact*.
///
/// The server has already picked a catalog key and its interpolation values
/// (`asale-server/src/i18n.rs`); dropping them here and forwarding only the
/// English sentence would leave every server-side error untranslatable in the
/// desktop app, even though both frontends ship the very same catalog. So the
/// envelope is re-wrapped, not flattened.
pub(crate) fn server_error(v: &Value, fallback: &str) -> CmdError {
    let e = &v["error"];
    let message = e["message"].as_str().unwrap_or(fallback).to_string();
    CmdError {
        message,
        key: e["key"].as_str().map(str::to_string),
        params: e["params"].as_object().cloned().unwrap_or_default(),
    }
}

/// Minimal query-component percent-encoding (chain ids are short ASCII).
pub(crate) fn urlencoding_simple(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                vec![c]
            } else {
                format!("%{:02X}", c as u32).chars().collect()
            }
        })
        .collect()
}
