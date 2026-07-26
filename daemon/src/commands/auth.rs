//! Account auth: register / sign in / sign out and the profile edits that
//! go with it.

use crate::keychain;
use crate::state::AppState;
use serde_json::{json, Value};
use super::server_client::{authed, finish_auth, resp_json};
use super::{R, err};

/// Register a new account on the server; stores tokens in the secret store.
pub async fn register(state: &AppState, email: String, password: String) -> R<Value> {
    let http = asale_client_core::http::plain();
    let resp = http
        .post(format!("{}/api/v1/auth/register", state.cfg.server_api_base))
        .json(&json!({"email": email, "password": password}))
        .send()
        .await
        .map_err(err)?;
    finish_auth(&resp_json(resp).await?).await
}

/// Log in; stores tokens in the secret store.
pub async fn login(state: &AppState, email: String, password: String) -> R<Value> {
    let http = asale_client_core::http::plain();
    let resp = http
        .post(format!("{}/api/v1/auth/login", state.cfg.server_api_base))
        .json(&json!({"email": email, "password": password}))
        .send()
        .await
        .map_err(err)?;
    finish_auth(&resp_json(resp).await?).await
}

/// Personal center: profile snapshot (name, avatar, linked providers, …).
pub async fn me_profile(state: &AppState) -> R<Value> {
    authed(state, reqwest::Method::GET, "/api/v1/me/profile", None).await
}

/// Update profile fields; only provided fields are sent.
pub async fn update_profile(state: &AppState, name: Option<String>, region: Option<String>) -> R<Value> {
    let mut body = serde_json::Map::new();
    if let Some(n) = name {
        body.insert("name".into(), Value::String(n));
    }
    if let Some(r) = region {
        body.insert("region".into(), Value::String(r));
    }
    authed(state, reqwest::Method::PATCH, "/api/v1/me/profile", Some(Value::Object(body))).await
}

/// Change the password (or set one for an OAuth-only account — no old password).
pub async fn change_password(state: &AppState, old_password: Option<String>, new_password: String) -> R<Value> {
    let mut body = serde_json::Map::new();
    if let Some(old) = old_password {
        body.insert("old_password".into(), Value::String(old));
    }
    body.insert("new_password".into(), Value::String(new_password));
    authed(state, reqwest::Method::POST, "/api/v1/me/password", Some(Value::Object(body))).await
}

/// Unlink a platform OAuth provider from the account.
pub async fn unlink_oauth(state: &AppState, provider: String) -> R<Value> {
    if provider != "google" && provider != "github" {
        return Err("unknown provider".to_string());
    }
    authed(state, reqwest::Method::DELETE, &format!("/api/v1/me/oauth/{provider}"), None).await
}

/// Drop the stored session tokens (local sign-out).
pub async fn logout() -> R<bool> {
    keychain::delete("access_token").map_err(err)?;
    keychain::delete("refresh_token").map_err(err)?;
    Ok(true)
}
