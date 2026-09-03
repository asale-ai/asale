//! Client configuration: the daemon's own identity, the upstream proxy
//! preference, free-form settings, and the consume-mode switch.

use crate::state::AppState;
use asale_client_core::http;
use serde_json::{json, Value};
use super::{R, err, now_secs};
use crate::cmd_err;

/// Return the client endpoint config.
pub fn client_config(state: &AppState) -> Value {
    json!({
        "server_api_base": state.cfg.server_api_base,
        "gateway_api_base": state.cfg.gateway_api_base,
        "gateway_ws_url": state.cfg.gateway_ws_url,
        "proxy_port": state.cfg.proxy_port,
        "device_id": state.device_id(),
    })
}

/// Daemon self-description (version, data dir, where it listens) — shown on the
/// Settings page in browser mode where the desktop shell's autostart/updater
/// don't exist.
///
/// The listen address is here because the Settings page offers "open this in a
/// browser", and that offer means two very different things depending on it: on
/// loopback the app is reachable only from this machine, on any other address
/// the daemon token is the only thing between the network and the user's
/// credentials. A page that hands out a URL has to be able to say which.
/// Whether the platform has refused this build as too old, and what it wants.
///
/// `null` when there is nothing to say, so the window can render the banner on
/// a truthy check without a second field meaning "but not really".
///
/// A poll rather than a push: the refusal happens in the publisher's connect
/// loop and in the local proxy, neither of which has a channel to the window,
/// and the shell is already polling this daemon every few seconds.
pub fn upgrade_notice() -> Value {
    match asale_client_core::upgrade::get() {
        Some(n) => json!({"current": n.current, "min": n.min, "path": n.path}),
        None => Value::Null,
    }
}

/// This seller's standing with the matcher: `{score, min_score, deprioritised}`,
/// or `null` before the gateway has reported one.
pub fn seller_status() -> Value {
    match asale_client_core::seller_status::get() {
        Some(s) => json!({
            "score": s.score,
            "min_score": s.min_score,
            "deprioritised": s.deprioritised(),
        }),
        None => Value::Null,
    }
}

pub fn daemon_info() -> Value {
    let bound = crate::bound_addr();
    json!({
        "name": "asaled",
        "version": env!("CARGO_PKG_VERSION"),
        "data_dir": crate::state::data_dir(),
        // Null before the listener is up (the very first RPCs of a cold start
        // reach this through the desktop shell's in-process daemon).
        "bind": bound.map(|a| a.to_string()),
        "port": bound.map(|a| a.port()),
        "remote": bound.map(|a| a.ip().is_unspecified() || !a.ip().is_loopback()),
    })
}

/// Settings key holding the encoded `ProxyPref`.
pub const PROXY_KEY: &str = "upstream_proxy";

/// Load the saved preference and apply it to the shared HTTP clients. Called at
/// daemon startup, before anything talks to a provider.
pub async fn load_proxy_pref(state: &AppState) -> anyhow::Result<()> {
    let saved = state.store.get_setting(PROXY_KEY).await?;
    http::set_preference(http::ProxyPref::from_setting(saved.as_deref()));
    Ok(())
}

/// Current proxy configuration for the Settings page.
pub fn proxy_settings() -> Value {
    let pref = http::preference();
    json!({
        "mode": pref.mode(),                       // auto | off | manual
        "url": match &pref {
            http::ProxyPref::Manual(u) => u.clone(),
            _ => String::new(),
        },
        // What the daemon would actually dial right now, however it was decided.
        "effective": http::upstream_proxy(),
        // Set in the environment, this outranks the saved preference; the UI
        // surfaces it so the setting never looks silently ignored.
        "env_override": http::env_override(),
        "env_var": http::PROXY_ENV,
    })
}

/// Persist a proxy preference and apply it immediately — no restart, because a
/// user fixing a blocked connection should not have to guess whether it took.
pub async fn set_proxy_settings(state: &AppState, mode: String, url: String) -> R<Value> {
    let pref = match mode.as_str() {
        "auto" => http::ProxyPref::Auto,
        "off" => http::ProxyPref::Direct,
        "manual" => {
            let url = url.trim().to_string();
            http::validate(&url)?;
            http::ProxyPref::Manual(url)
        }
        other => {
            return Err(cmd_err!(
                "errors.settings.unknownProxyMode",
                format!("unknown proxy mode: {other}"),
                value = other
            ))
        }
    };
    state.store.set_setting(PROXY_KEY, &pref.to_setting()).await.map_err(err)?;
    http::set_preference(pref);
    tracing::info!("upstream proxy set to {:?}", http::upstream_proxy());
    Ok(proxy_settings())
}

/// Check whether provider endpoints are reachable through a candidate proxy —
/// `mode`/`url` describe what to test, so a user can verify *before* saving.
///
/// The probe asks Anthropic's OAuth token endpoint to refresh a deliberately
/// invalid token. Any answer naming the token means we got through; a 403 means
/// the connection itself was refused (region block). Nothing real is sent, so
/// this can never rotate or invalidate a stored credential.
pub async fn test_proxy(mode: String, url: String) -> R<Value> {
    let candidate = match mode.as_str() {
        // What auto *would* pick, not what the saved preference resolves to:
        // the point is to check the mode on screen before it is committed.
        "auto" => http::auto_proxy(),
        "off" => None,
        "manual" => {
            let url = url.trim().to_string();
            http::validate(&url)?;
            Some(url)
        }
        other => {
            return Err(cmd_err!(
                "errors.settings.unknownProxyMode",
                format!("unknown proxy mode: {other}"),
                value = other
            ))
        }
    };
    let started = std::time::Instant::now();
    let resp = http::build(candidate.as_deref())
        .post("https://api.anthropic.com/v1/oauth/token")
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": "asale-connectivity-probe-invalid-token",
            "client_id": crate::oauth::claude_client_id(),
        }))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await;
    let ms = started.elapsed().as_millis() as u64;
    let (ok, detail) = match resp {
        // 403 is the region block answering; every other status means the
        // endpoint itself replied and only rejected the dummy token.
        Ok(r) if r.status() == reqwest::StatusCode::FORBIDDEN => (false, "blocked".to_string()),
        Ok(r) => return Ok(json!({ "ok": true, "detail": r.status().as_u16().to_string(), "ms": ms, "proxy": candidate })),
        Err(e) => (false, e.to_string()),
    };
    Ok(json!({ "ok": ok, "detail": detail, "ms": ms, "proxy": candidate }))
}

/// Settings passthrough.
pub async fn get_setting(state: &AppState, key: String) -> R<Option<String>> {
    state.store.get_setting(&key).await.map_err(err)
}

pub async fn set_setting(state: &AppState, key: String, value: String) -> R<()> {
    state.store.set_setting(&key, &value).await.map_err(err)
}

/// Set the consumer routing mode: direct | market | auto (persisted).
pub async fn consume_set_mode(state: &AppState, mode: String) -> R<Value> {
    if !matches!(mode.as_str(), "direct" | "market" | "auto") {
        return Err(cmd_err!("errors.settings.badConsumeMode", "mode must be direct | market | auto"));
    }
    state.store.set_setting("consume_mode", &mode).await.map_err(err)?;
    consume_get_mode(state).await
}

/// Current mode plus which providers could serve direct right now — the UI
/// uses this to explain the effective route.
pub async fn consume_get_mode(state: &AppState) -> R<Value> {
    let mode = state
        .store
        .get_setting("consume_mode")
        .await
        .map_err(err)?
        .unwrap_or_else(|| "market".to_string());
    let now = now_secs();
    let (claude, gemini) = {
        let pool = state.pool.lock().map_err(|_| "pool lock poisoned".to_string())?;
        (
            asale_protocol::ids::Provider::ALL.iter().filter(|p| p.is_claude_family()).any(|p| pool.any_available(p.as_str(), now)),
            pool.any_available("gemini", now),
        )
    };
    Ok(json!({
        "mode": mode,
        "direct_available": {"claude": claude, "gemini": gemini},
    }))
}
