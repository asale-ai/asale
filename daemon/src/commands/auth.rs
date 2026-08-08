//! Account auth: register / sign in / sign out and the profile edits that
//! go with it.

use crate::keychain;
use crate::state::AppState;
use serde_json::{json, Value};
use super::server_client::{authed, finish_auth, server_error, resp_json};
use super::{R, err};
use crate::cmd_err;

/// Register a new account on the server; stores tokens in the secret store.
///
/// `region` is the ISO 3166-1 alpha-2 country picked on the sign-up screen. The
/// server rejects a missing or malformed one: nothing about a user's location is
/// ever inferred, so sign-up is the only place it can be established.
///
/// Registering does **not** sign the user in: the server withholds the token
/// pair until the link it mails has been clicked. The answer is therefore a
/// pending state for the UI to render, not a session — passing it to
/// `finish_auth` would read the absent tokens as an authentication failure and
/// report "auth failed" for a registration that in fact succeeded.
pub async fn register(state: &AppState, email: String, password: String, region: String) -> R<Value> {
    let http = asale_client_core::http::plain();
    let resp = http
        .post(format!("{}/api/v1/auth/register", state.cfg.server_api_base))
        // `device_fp` carries this installation's device id, which is what the
        // server's one-reward-per-device rule keys on. Sent even though the
        // desktop app has no invite-code field yet: the moment it grows one,
        // forgetting this would make the desktop the way around the rule.
        .json(&json!({
            "email": email,
            "password": password,
            "region": region,
            "device_fp": [state.device_id.clone()],
        }))
        .send()
        .await
        .map_err(err)?;
    let v = resp_json(resp).await?;
    if v.get("verification_required").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(json!({
            "verification_required": true,
            "verification_sent": v.get("verification_sent").and_then(Value::as_bool).unwrap_or(false),
            "user_id": v["user_id"],
            "email": v["email"],
        }));
    }
    // No gate configured on this server — the old immediate-session contract.
    finish_auth(&v).await
}

/// Ask the server to mail a fresh verification link.
///
/// Needed on the desktop client too: without it a user whose verification mail
/// never arrived has no way forward from this app at all, since the account
/// exists (so re-registering is refused) but cannot sign in.
pub async fn resend_verification(state: &AppState, email: String) -> R<Value> {
    let http = asale_client_core::http::plain();
    let resp = http
        .post(format!("{}/api/v1/auth/resend-verification", state.cfg.server_api_base))
        .json(&json!({"email": email}))
        .send()
        .await
        .map_err(err)?;
    resp_json(resp).await
}

/// Log in; stores tokens in the secret store.
///
/// An unverified address is reported as the same `verification_required` shape
/// `register` returns, rather than as an error. The password was correct and the
/// account exists — the user needs the resend button, and `resp_json` would
/// flatten the server's `email_unverified` code into a bare message the UI
/// cannot branch on.
pub async fn login(state: &AppState, email: String, password: String) -> R<Value> {
    let http = asale_client_core::http::plain();
    let resp = http
        .post(format!("{}/api/v1/auth/login", state.cfg.server_api_base))
        .json(&json!({"email": email, "password": password}))
        .send()
        .await
        .map_err(err)?;
    let status = resp.status();
    let v: Value = resp.json().await.map_err(err)?;
    if !status.is_success() {
        if v["error"]["code"].as_str() == Some("email_unverified") {
            return Ok(json!({"verification_required": true, "email": email}));
        }
        return Err(server_error(&v, "request failed"));
    }
    finish_auth(&v).await
}

/// Personal center: profile snapshot (name, avatar, linked providers, …).
pub async fn me_profile(state: &AppState) -> R<Value> {
    authed(state, reqwest::Method::GET, "/api/v1/me/profile", None).await
}

/// This account's invite code, link and referral numbers.
///
/// A straight passthrough, and the only reason the client needs it: the share
/// card carries the invite link as a QR, and the code is minted server-side on
/// first read — so there is nothing local to derive it from.
pub async fn me_referral(state: &AppState) -> R<Value> {
    authed(state, reqwest::Method::GET, "/api/v1/me/referral", None).await
}

/// Update profile fields; only provided fields are sent.
pub async fn update_profile(
    state: &AppState,
    name: Option<String>,
    region: Option<String>,
    avatar_url: Option<String>,
) -> R<Value> {
    let mut body = serde_json::Map::new();
    if let Some(n) = name {
        body.insert("name".into(), Value::String(n));
    }
    if let Some(r) = region {
        body.insert("region".into(), Value::String(r));
    }
    if let Some(a) = avatar_url {
        body.insert("avatar_url".into(), Value::String(a));
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
        return Err(cmd_err!("errors.oauth.unknownProvider", "unknown provider", provider = provider.as_str()));
    }
    authed(state, reqwest::Method::DELETE, &format!("/api/v1/me/oauth/{provider}"), None).await
}

/// Drop the stored session tokens (local sign-out).
///
/// The consumer API key goes with them: it belongs to the account that just
/// signed out, so leaving it cached would have the next account's market
/// traffic billed to the previous one — or, once the key is revoked, 401 with
/// no way back short of hitting "regenerate" by hand.
pub async fn logout(state: &AppState) -> R<bool> {
    keychain::delete("access_token").map_err(err)?;
    keychain::delete("refresh_token").map_err(err)?;
    super::wallet::forget_key(state).await?;
    Ok(true)
}
