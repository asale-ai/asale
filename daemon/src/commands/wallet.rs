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

/// Settings key holding the cached consumer API key.
pub(crate) const KEY_SETTING: &str = "asale_api_key";
/// Settings key holding the server that key was minted against.
const KEY_ORIGIN_SETTING: &str = "asale_api_key_origin";

/// Cache a freshly minted key, stamped with the server that issued it.
pub(crate) async fn remember_key(state: &AppState, key: &str) -> R<()> {
    state.store.set_setting(KEY_SETTING, key).await.map_err(err)?;
    state.store.set_setting(KEY_ORIGIN_SETTING, &state.cfg.server_api_base).await.map_err(err)?;
    *state.asale_key.write().await = Some(key.to_string());
    Ok(())
}

/// Drop the cached key everywhere, so the next `ensure_*` mints a new one.
/// Called on sign-out and whenever the gateway tells us the key is unknown.
pub(crate) async fn forget_key(state: &AppState) -> R<()> {
    state.store.set_setting(KEY_SETTING, "").await.map_err(err)?;
    state.store.set_setting(KEY_ORIGIN_SETTING, "").await.map_err(err)?;
    *state.asale_key.write().await = None;
    Ok(())
}

/// The cached key, but only if it belongs to the server this build talks to.
///
/// A key is scoped to the account *and the deployment* that issued it, while
/// `~/.asale/asale.db` is not: a dev build (`ASALE_SERVER_API=127.0.0.1:9090`)
/// and the packaged one (`api.asale.ai`) share the same store on one machine.
/// Without this check the first build's key survives the switch and every
/// market request 401s with `unknown api key` — from the tool's point of view
/// the proxy is simply broken, with nothing on screen saying why.
///
/// An unstamped key predates this check; re-stamp it rather than forcing a
/// pointless re-mint on upgrade.
pub(crate) async fn cached_key(state: &AppState) -> R<Option<String>> {
    let key = state.store.get_setting(KEY_SETTING).await.map_err(err)?.unwrap_or_default();
    if key.is_empty() {
        return Ok(None);
    }
    match state.store.get_setting(KEY_ORIGIN_SETTING).await.map_err(err)?.unwrap_or_default() {
        o if o.is_empty() => {
            state.store.set_setting(KEY_ORIGIN_SETTING, &state.cfg.server_api_base).await.map_err(err)?;
        }
        o if o != state.cfg.server_api_base => {
            tracing::warn!(
                "cached asale api key was minted against {o}, this build talks to {} — discarding it",
                state.cfg.server_api_base
            );
            forget_key(state).await?;
            return Ok(None);
        }
        _ => {}
    }
    Ok(Some(key))
}

/// Mint a fresh consumer API key on the server and wire it into the local
/// proxy, replacing any previous one. Used by the "regenerate" action, and by
/// the self-heal path when the gateway rejects the cached key. The key is
/// cached in the local store (on disk), not the secret store, so persisting it
/// never triggers an OS credential prompt.
pub async fn create_api_key(state: &AppState, label: String) -> R<Value> {
    let mut v = authed(state, reqwest::Method::POST, "/api/v1/apikeys", Some(json!({"label": label}))).await?;
    if let Some(key) = v["key"].as_str().map(String::from) {
        remember_key(state, &key).await?;
        // The tools already buying hold the key this one just replaced; rewrite
        // their configs before they get a 401 out of it.
        let refreshed = super::buy::refresh_buy_tool_keys(state, &key).await?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("refreshed_tools".into(), json!(refreshed));
        }
    }
    Ok(v)
}

/// Return the cached asale key, minting one on the server if none exists yet.
/// Called automatically when the client starts / the account view opens, so the
/// user never has to generate a key by hand. Returns `{ "key": null }` when not
/// yet signed in (a key can only be minted for a logged-in account).
pub async fn ensure_api_key(state: &AppState) -> R<Value> {
    if let Some(k) = cached_key(state).await? {
        *state.asale_key.write().await = Some(k.clone());
        return Ok(json!({ "key": k }));
    }
    if keychain::get("access_token").map_err(err)?.is_none() {
        return Ok(json!({ "key": Value::Null }));
    }
    // Adopt the account's default before minting anything — see
    // `apikeys::adopt_default`. Best-effort: a server that cannot answer falls
    // through to minting, which is what this always did.
    match super::apikeys::adopt_default(state).await {
        Ok(Some(k)) => return Ok(json!({ "key": k })),
        Ok(None) => {}
        Err(e) => tracing::debug!("could not adopt the account's default api key: {e}"),
    }
    create_api_key(state, "asale".into()).await
}

/// Load the cached asale key into the running proxy (on startup).
pub async fn load_api_key(state: &AppState) -> R<bool> {
    match cached_key(state).await? {
        Some(key) => {
            *state.asale_key.write().await = Some(key);
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Fetch the server-derived deposit address for a chain (server §10; 503 when
/// the deposit service is not provisioned is passed through as the error).
pub async fn wallet_deposit_address(state: &AppState, chain: String) -> R<Value> {
    let path = format!("/api/v1/wallet/deposit-address?chain={}", urlencoding_simple(&chain));
    authed(state, reqwest::Method::POST, &path, None).await
}

/// Open a scan-to-pay session (server §10). `amount` is micro-USDT to prefill
/// in the payment QR, or `None` for "any amount".
///
/// The session only tracks a top-up so the wallet screen can announce the
/// arrival itself; it holds no funds and the deposit is credited identically
/// without one.
pub async fn wallet_deposit_session(state: &AppState, chain: String, amount: Option<i64>) -> R<Value> {
    authed(
        state,
        reqwest::Method::POST,
        "/api/v1/wallet/deposit-session",
        Some(json!({"chain": chain, "amount": amount})),
    )
    .await
}

/// Open a card top-up: a Dodo checkout for `amount`, plus a session to poll.
///
/// The reply carries `checkout_url` — the hosted page the user pays on — and
/// the `fee`/`charge` breakdown, which is the server's own arithmetic rather
/// than a client-side copy of it: the figure shown before the card is presented
/// has to be the figure that was actually asked for.
///
/// Nothing is credited here. The wallet moves when the processor's webhook
/// reaches the server, which is why the same `wallet_deposit_session_get` poll
/// is what tells the sheet it landed.
/// `open_local` asks the daemon to open the checkout in this machine's browser,
/// the way the OAuth flows do. The Tauri webview cannot host a third party's
/// payment page — its CSP forbids the frame, and a card form inside somebody
/// else's shell is the shape a phishing page has — so the desktop build pays in
/// a real browser and watches the session from here.
pub async fn wallet_card_session(state: &AppState, amount: Option<i64>, open_local: bool) -> R<Value> {
    let out = authed(
        state,
        reqwest::Method::POST,
        "/api/v1/wallet/card-session",
        Some(json!({ "amount": amount })),
    )
    .await?;
    if open_local {
        if let Some(url) = out.get("checkout_url").and_then(|u| u.as_str()) {
            // Best effort: the URL is in the reply either way, and the sheet
            // shows it as a link when this does nothing (a headless box).
            if let Err(e) = open::that_detached(url) {
                tracing::warn!("could not open the checkout page: {e}");
            }
        }
    }
    Ok(out)
}

/// Poll one session. Each read advances it against what is on chain, so this
/// call is the whole detection mechanism — there is no background job.
pub async fn wallet_deposit_session_get(state: &AppState, session_ref: String) -> R<Value> {
    let path = format!("/api/v1/wallet/deposit-session/{}", urlencoding_simple(&session_ref));
    authed(state, reqwest::Method::GET, &path, None).await
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
