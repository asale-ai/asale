//! Sell side: the publishing switch and its supervisor state, device list,
//! and the publisher policy pulled from the server.

use crate::keychain;
use crate::auth_store;
use crate::publisher;
use crate::state::AppState;
use crate::tool_config;
use serde_json::{json, Value};
use super::buy::{buy_is_enabled};
use super::server_client::{authed};
use super::{R, err, now_secs};

/// Publisher on/off (spec §5/§9). `on` starts the real WS relay session (device
/// registration → signed handshake → supply declaration → task serving, with
/// auto-reconnect); `off` disconnects gracefully.
///
/// There is no global sell switch: this is never called directly by the UI.
/// The session follows the per-account switches (see `accounts_changed`) and is
/// re-asserted by the startup supervisor.
pub async fn set_publishing(state: &AppState, on: bool) -> R<bool> {
    use asale_client_core::ConnState;
    if on {
        // Idempotent: if a live session exists, leave it. `Kicked` is not
        // live — the session loop has already returned and will never
        // reconnect on its own — so calling this again is how a device comes
        // back after the kick's cause is cleared.
        {
            let guard = state.publisher.read().await;
            if guard
                .as_ref()
                .map(|h| h.state())
                .is_some_and(|s| !matches!(s, ConnState::Offline | ConnState::Kicked))
            {
                return Ok(true);
            }
        }
        let handle = publisher::start(state).await.map_err(err)?;
        *state.publisher.write().await = Some(handle);
        // Ensure the token-refresh loop is running while we publish.
        {
            let mut rt = state.refresh_task.write().await;
            if rt.is_none() {
                *rt = Some(publisher::spawn_refresh_loop(state.store.clone(), state.pool.clone()));
            }
        }
        Ok(true)
    } else {
        if let Some(handle) = state.publisher.write().await.take() {
            handle.stop();
        }
        Ok(false)
    }
}

/// Live proxy + publisher status. `publish_state` reflects the real WS session
/// (offline/connecting/reconnecting/online/throttled/kicked), not a local flag.
///
/// `publish_wanted` is the persisted switch, which is a different question:
/// "should this device be selling" survives restarts and outages, while
/// `publish_state` says whether it currently is.
pub async fn proxy_status(state: &AppState) -> R<Value> {
    let publish_state = match state.publisher.read().await.as_ref() {
        Some(h) => h.state().as_str().to_string(),
        None => "offline".to_string(),
    };
    let publishing = matches!(publish_state.as_str(), "online" | "throttled");
    Ok(json!({
        "port": state.cfg.proxy_port,
        "api_key_loaded": state.asale_key.read().await.is_some(),
        "publishing": publishing,
        "publish_state": publish_state,
        "publish_wanted": publish_wanted(state).await,
    }))
}

/// Whether this device should be on the market at all.
///
/// Derived, never stored: selling is a per-account decision, so "should this
/// device be selling" is exactly "is any account switched on". A separate
/// global flag could only ever disagree with the switches the user actually
/// sees — a device left off while every account reads *on*, or the reverse.
pub async fn publish_wanted(state: &AppState) -> bool {
    matches!(state.store.list_tools().await, Ok(tools) if tools.iter().any(|t| t.sell_enabled))
}

/// Everything the always-on status widget shows, in one round trip.
///
/// The widget is global — it polls from every page — so fanning out to four
/// commands per tick is what this exists to avoid. It also answers a question
/// no single existing command does: not just "is the link up", but whether
/// being up is currently worth anything (signed in, accounts selling, lanes
/// actually serving).
///
/// `upgrade` and `reputation` are here for the second question rather than the
/// first, and are the two answers a headless install could not otherwise get:
/// the desktop window polls them separately to draw a banner, and on a server
/// there is no window, so `asale status` reads them from here. Both are the same
/// shape the dedicated commands return — see `settings::upgrade_notice` and
/// `settings::seller_status` — and both are `null` when there is nothing to say.
pub async fn client_status(state: &AppState) -> R<Value> {
    let publish_state = match state.publisher.read().await.as_ref() {
        Some(h) => h.state().as_str().to_string(),
        None => "offline".to_string(),
    };
    let tools = state.store.list_tools().await.map_err(err)?;
    let selling: Vec<Value> = tools
        .iter()
        .filter(|t| t.sell_enabled)
        .map(|t| json!({ "provider": t.provider, "account_id": t.account_id }))
        .collect();

    // Lane counts are what explain a device that is online and still earning
    // nothing: every lane of every selling account can be cooling or paused.
    let (lanes_selling, lanes_blocked) = {
        let pool = state.pool.lock().map_err(|_| "pool lock poisoned".to_string())?;
        let views = pool.lane_views(now_secs());
        let selling = views.iter().filter(|l| l.status == "selling").count();
        let blocked = views.iter().filter(|l| l.sell_enabled && l.status != "selling").count();
        (selling, blocked)
    };

    let mut buying: Vec<&str> = Vec::new();
    for tool in tool_config::TOOLS {
        if buy_is_enabled(&state.store, tool).await {
            buying.push(tool_config::label(tool));
        }
    }

    Ok(json!({
        "publish_state": publish_state,
        "signed_in": keychain::get("access_token").map_err(err)?.is_some(),
        "api_key_loaded": state.asale_key.read().await.is_some(),
        "proxy_port": state.cfg.proxy_port,
        "device_id": state.device_id(),
        "server_api_base": state.cfg.server_api_base,
        "gateway_ws_url": state.cfg.gateway_ws_url,
        "accounts_total": tools.len(),
        "selling": selling,
        "lanes_selling": lanes_selling,
        "lanes_blocked": lanes_blocked,
        "buying": buying,
        "version": env!("CARGO_PKG_VERSION"),
        "upgrade": super::settings::upgrade_notice(),
        "reputation": super::settings::seller_status(),
    }))
}

/// Rebuild the account pool, bring the market session in line with the
/// switches, and tell a live session that what this device offers has changed.
///
/// All three matter: the pool decides which account serves the next request,
/// the session must exist at all for any of it to be visible, and the nudge is
/// what puts the new offering in front of the market. Without the nudge a
/// switched-off account keeps receiving work until the next periodic
/// re-declaration.
///
/// Syncing the session here is what replaces the old global switch: turning on
/// the first account connects the device, turning off the last one disconnects
/// it, so the user never has to arm two switches to sell one account.
pub async fn accounts_changed(state: &AppState) {
    publisher::rebuild_pool(&state.store, &state.pool).await;
    // Keep `~/.asale/auths` a faithful picture of the `tools` table. Derived
    // here, from the one function every account change already goes through,
    // rather than at each individual call site — see `auth_store::resync`.
    auth_store::resync(&state.store).await;
    let wanted = publish_wanted(state).await;
    // Not signed in yet is the ordinary failure here; the startup supervisor
    // retries every minute, so this is a debug line, not a warning.
    if let Err(e) = set_publishing(state, wanted).await {
        tracing::debug!("publish sync (wanted={wanted}) deferred: {e}");
    }
    if let Some(h) = state.publisher.read().await.as_ref() {
        h.nudge();
    }
}

/// One account's credential has just been replaced — release whatever the old
/// one's failure was holding.
///
/// Call this on every path that writes a fresh token for an existing account:
/// the loopback OAuth callback, the device flow, a re-import from a local CLI.
///
/// The order is the whole point. `rebuild_pool` re-applies persisted pauses over
/// the pool it has just rebuilt, and `set_accounts` carries `auth_failed`
/// forward, so clearing either one alone is undone within the minute: the disk
/// row has to go first, then the in-memory flag, and only then may the pool be
/// rebuilt. That is why this is not folded into [`accounts_changed`] — its
/// callers are the ones that know a *credential* changed, as opposed to a
/// switch or a limit.
pub async fn credential_replaced(state: &AppState, provider: &str, account_id: &str) {
    let cleared = state
        .store
        .clear_lane_pause_reason(provider, account_id, "auth")
        .await
        .unwrap_or_default();
    let in_pool = state
        .pool
        .lock()
        .map(|mut p| p.clear_auth_failure(provider, account_id))
        .unwrap_or_default();
    if cleared.is_empty() && in_pool.is_empty() {
        return;
    }
    tracing::info!(
        provider, account_id,
        models = in_pool.len().max(cleared.len()),
        "a fresh credential cleared this account's authentication pause"
    );
    if let Some(h) = state.publisher.read().await.as_ref() {
        for m in in_pool.iter().chain(cleared.iter().filter(|m| !in_pool.contains(m))) {
            h.resume(m);
        }
    }
}

/// What the server currently believes this account's devices are selling.
///
/// The authoritative counterpart to the local publish state: it answers "is the
/// market actually seeing me, and with which models" from the side that
/// matters, which is the only way to catch a client that thinks it is online
/// against a gateway that disagrees.
pub async fn devices_list(state: &AppState) -> R<Value> {
    let mut v = authed(state, reqwest::Method::GET, "/api/v1/me/devices", None).await?;
    // Mark this install's own row so the UI can tell it from the user's other
    // machines without the frontend having to know how device ids are minted.
    if let Some(devices) = v["devices"].as_array_mut() {
        for d in devices {
            let is_self = d["device_id"].as_str() == Some(state.device_id().as_str());
            d["this_device"] = json!(is_self);
        }
    }
    Ok(v)
}

/// Read the publisher policy: the server's copy is authoritative, and pulling
/// it also refreshes the local mirror. If the server cannot be reached the
/// mirror answers, tagged with the error so the UI can say it is showing a
/// cached value.
pub async fn publish_policy_get(state: &AppState) -> R<Value> {
    match authed(state, reqwest::Method::GET, "/api/v1/me/publish-config", None).await {
        Ok(v) => {
            let policy: publisher::PublishPolicy =
                serde_json::from_value(v["config"].clone()).unwrap_or_default();
            publisher::store_policy(&state.store, &policy).await.map_err(err)?;
            Ok(json!({"policy": policy, "source": "server"}))
        }
        Err(e) => Ok(json!({
            "policy": publisher::local_policy(&state.store).await,
            "source": "cache",
            "server_error": e.message,
        })),
    }
}

/// Update the publisher policy. The server validates and stores it; only then
/// is the mirror updated and the live session nudged, so the local copy can
/// never claim a policy the server rejected.
pub async fn publish_policy_set(state: &AppState, patch: Value) -> R<Value> {
    let mut policy = publisher::local_policy(&state.store).await;
    if let Some(v) = patch.get("reserve_floor").and_then(Value::as_i64) {
        policy.reserve_floor = v;
    }
    if let Some(v) = patch.get("max_rpm").and_then(Value::as_i64) {
        policy.max_rpm = v as i32;
    }
    if let Some(v) = patch.get("min_price").and_then(Value::as_i64) {
        policy.min_price = v;
    }
    for (key, list) in [
        ("model_whitelist", &mut policy.model_whitelist),
        ("model_blacklist", &mut policy.model_blacklist),
    ] {
        if let Some(arr) = patch.get(key).and_then(Value::as_array) {
            *list = arr.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        }
    }
    let body = serde_json::to_value(&policy).map_err(err)?;
    let v = authed(state, reqwest::Method::PUT, "/api/v1/me/publish-config", Some(body)).await?;
    let stored: publisher::PublishPolicy = serde_json::from_value(v["config"].clone()).unwrap_or(policy);
    publisher::store_policy(&state.store, &stored).await.map_err(err)?;
    accounts_changed(state).await;
    Ok(json!({"policy": stored, "source": "server"}))
}
