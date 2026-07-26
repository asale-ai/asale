//! Wallet: balance, history, deposit address, withdrawal, and the consumer
//! API keys the buy side presents to the local proxy.

use crate::keychain;
use crate::state::AppState;
use serde_json::{json, Value};
use super::server_client::{authed, urlencoding_simple};
use super::{R, err};

/// Wallet overview from the server (requires prior login).
pub async fn wallet_overview(state: &AppState) -> R<Value> {
    authed(state, reqwest::Method::GET, "/api/v1/me/wallet", None).await
}

/// Deposit + withdrawal history for the wallet screen.
pub async fn wallet_history(state: &AppState) -> R<Value> {
    authed(state, reqwest::Method::GET, "/api/v1/me/wallet/history", None).await
}

/// Mint a fresh consumer API key on the server and wire it into the local
/// proxy, replacing any previous one. Used by the "regenerate" action. The key
/// is cached in the local store (on disk), not the secret store, so persisting
/// it never triggers an OS credential prompt.
pub async fn create_api_key(state: &AppState, label: String) -> R<Value> {
    let v = authed(state, reqwest::Method::POST, "/api/v1/apikeys", Some(json!({"label": label}))).await?;
    if let Some(key) = v["key"].as_str() {
        state.store.set_setting("asale_api_key", key).await.map_err(err)?;
        *state.asale_key.write().await = Some(key.to_string());
    }
    Ok(v)
}

/// Return the cached asale key, minting one on the server if none exists yet.
/// Called automatically when the client starts / the account view opens, so the
/// user never has to generate a key by hand. Returns `{ "key": null }` when not
/// yet signed in (a key can only be minted for a logged-in account).
pub async fn ensure_api_key(state: &AppState) -> R<Value> {
    if let Some(k) = state.store.get_setting("asale_api_key").await.map_err(err)? {
        if !k.is_empty() {
            *state.asale_key.write().await = Some(k.clone());
            return Ok(json!({ "key": k }));
        }
    }
    if keychain::get("access_token").map_err(err)?.is_none() {
        return Ok(json!({ "key": Value::Null }));
    }
    let v = authed(state, reqwest::Method::POST, "/api/v1/apikeys", Some(json!({ "label": "asale" }))).await?;
    if let Some(key) = v["key"].as_str() {
        state.store.set_setting("asale_api_key", key).await.map_err(err)?;
        *state.asale_key.write().await = Some(key.to_string());
    }
    Ok(v)
}

/// Load the cached asale key into the running proxy (on startup).
pub async fn load_api_key(state: &AppState) -> R<bool> {
    if let Some(key) = state.store.get_setting("asale_api_key").await.map_err(err)? {
        *state.asale_key.write().await = Some(key);
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Fetch the server-derived deposit address for a chain (server §10; 503 when
/// the deposit service is not provisioned is passed through as the error).
pub async fn wallet_deposit_address(state: &AppState, chain: String) -> R<Value> {
    let path = format!("/api/v1/wallet/deposit-address?chain={}", urlencoding_simple(&chain));
    authed(state, reqwest::Method::POST, &path, None).await
}

/// Submit a withdrawal request (amount in micro-USDT). Risk checks, signing
/// and broadcast happen server-side; errors (402 insufficient balance, limits)
/// pass through as messages.
pub async fn wallet_withdraw(state: &AppState, chain: String, to_address: String, amount: i64) -> R<Value> {
    authed(
        state,
        reqwest::Method::POST,
        "/api/v1/wallet/withdraw",
        Some(json!({"chain": chain, "to_address": to_address, "amount": amount})),
    )
    .await
}
