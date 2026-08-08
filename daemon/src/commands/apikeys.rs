//! Consumer API keys, as the desktop app manages them.
//!
//! The server owns the keys; this module owns the one thing the server cannot
//! see — **which key the tools on this machine are actually holding**. That is
//! `~/.claude/settings.json`, `~/.codex/auth.json` and friends, written by the
//! buy switch, and nothing on the web can reach it.
//!
//! So the interesting operation here is not "create a key" but *adopting* one:
//! caching it locally, loading it into the running proxy, and rewriting every
//! buying tool's config in one step. Moving the account's default key is the
//! moment that matters — the UI asks first, because rewriting somebody's tool
//! configs behind their back is not a thing to do silently, and because leaving
//! them un-rewritten is how every tool starts answering 401 an hour later.

use super::server_client::authed;
use super::{err, R};
use crate::state::AppState;
use crate::tool_config;
use serde_json::{json, Value};

/// The masked form the server stores in `api_keys.key_preview`.
///
/// Recomputed here rather than round-tripped so the daemon can answer "is the
/// key I am holding the one in row 3" without ever sending the key anywhere.
/// It mirrors `security::api_key_preview` on the server; a drift between the
/// two costs a wrong "in use" marker and nothing else, which is why this is a
/// tolerable copy and the authentication path is not.
fn preview(key: &str) -> String {
    let body = key.strip_prefix("sk-asale-").unwrap_or(key);
    if body.len() <= 8 {
        return format!("sk-asale-{}", "•".repeat(body.len()));
    }
    format!("sk-asale-{}••••{}", &body[..4], &body[body.len() - 4..])
}

/// Which locally installed CLIs are buying right now.
///
/// This is the list the UI needs *before* it moves the default key: the whole
/// prompt is "these tools are pointed at asale, shall I re-key them too", and a
/// prompt that cannot name them is not worth showing.
pub(crate) async fn buying_tools(state: &AppState) -> R<Vec<Value>> {
    let mut out = Vec::new();
    for tool in tool_config::TOOLS {
        if state.store.buy_tool(tool).await.map_err(err)?.enabled {
            // The label travels with the id: these are product names the daemon
            // owns (`tool_config::label`), not strings the frontend translates.
            out.push(json!({ "id": tool, "label": tool_config::label(tool) }));
        }
    }
    Ok(out)
}

/// GET /api/v1/apikeys, plus the two facts only this machine knows: which row
/// the local proxy is holding, and which tools would need re-keying.
pub async fn list_api_keys(state: &AppState) -> R<Value> {
    let mut v = authed(state, reqwest::Method::GET, "/api/v1/apikeys", None).await?;
    let held = super::wallet::cached_key(state).await?.map(|k| preview(&k));
    if let Some(keys) = v["keys"].as_array_mut() {
        for k in keys.iter_mut() {
            let mine = held.as_deref().is_some_and(|p| k["key_preview"].as_str() == Some(p));
            k["in_use"] = json!(mine);
        }
    }
    if let Some(obj) = v.as_object_mut() {
        obj.insert("buying_tools".into(), json!(buying_tools(state).await?));
        obj.insert("has_local_key".into(), json!(held.is_some()));
    }
    Ok(v)
}

/// Mint a key. `apply` additionally adopts it here — cache, proxy, tool configs.
///
/// Creating a key and using it are separate on purpose: most keys minted from
/// this page are for somebody's own code, and hijacking the tools' credential
/// every time one is created is exactly the surprise this whole feature exists
/// to remove.
pub async fn create_api_key_ex(
    state: &AppState,
    label: String,
    expires_in_days: Option<i64>,
    set_default: bool,
    apply: bool,
) -> R<Value> {
    let mut v = authed(
        state,
        reqwest::Method::POST,
        "/api/v1/apikeys",
        Some(json!({
            "label": label,
            "expires_in_days": expires_in_days,
            "set_default": set_default,
        })),
    )
    .await?;
    if apply {
        if let Some(key) = v["key"].as_str().map(String::from) {
            let refreshed = adopt(state, &key).await?;
            if let Some(obj) = v.as_object_mut() {
                obj.insert("refreshed_tools".into(), json!(refreshed));
            }
        }
    }
    Ok(v)
}

/// PATCH one key. When the default moves and `apply` is set, the new default is
/// read back in plaintext and written into every buying tool.
#[allow(clippy::too_many_arguments)]
pub async fn update_api_key(
    state: &AppState,
    id: i64,
    label: Option<String>,
    enabled: Option<bool>,
    // Doubly wrapped for the same reason the server's field is: absent leaves
    // the expiry alone, `Some(None)` clears it, `Some(Some(n))` re-dates it.
    expires_in_days: Option<Option<i64>>,
    set_default: Option<bool>,
    apply: bool,
) -> R<Value> {
    let mut body = serde_json::Map::new();
    if let Some(l) = label {
        body.insert("label".into(), json!(l));
    }
    if let Some(e) = enabled {
        body.insert("enabled".into(), json!(e));
    }
    if let Some(d) = expires_in_days {
        body.insert("expires_in_days".into(), json!(d));
    }
    if let Some(d) = set_default {
        body.insert("set_default".into(), json!(d));
    }
    let mut v = authed(
        state,
        reqwest::Method::PATCH,
        &format!("/api/v1/apikeys/{id}"),
        Some(Value::Object(body)),
    )
    .await?;
    if apply && v["key"]["is_default"].as_bool().unwrap_or(false) {
        let refreshed = apply_api_key(state, id).await?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("refreshed_tools".into(), refreshed["refreshed_tools"].clone());
        }
    }
    Ok(v)
}

/// DELETE one key.
///
/// If the deleted key was the one this machine was holding, the local cache is
/// dropped with it: keeping it would leave the proxy authenticating with a row
/// that no longer exists, and the self-heal path would only rediscover that on
/// the next 401 — after a tool has already failed in front of the user.
pub async fn delete_api_key(state: &AppState, id: i64) -> R<Value> {
    let held_preview = super::wallet::cached_key(state).await?.map(|k| preview(&k));
    let deleted_preview = key_preview_of(state, id).await;
    let v = authed(state, reqwest::Method::DELETE, &format!("/api/v1/apikeys/{id}"), None).await?;
    if held_preview.is_some() && held_preview == deleted_preview {
        super::wallet::forget_key(state).await?;
    }
    Ok(v)
}

/// The plaintext of one key, for the owner to copy.
pub async fn reveal_api_key(state: &AppState, id: i64) -> R<Value> {
    authed(state, reqwest::Method::POST, &format!("/api/v1/apikeys/{id}/reveal"), None).await
}

/// Make one key the credential this machine's tools use, without changing which
/// key the account calls its default.
///
/// Reads the plaintext back from the server, which is the only way the desktop
/// app can hold a key it did not mint itself — a key created in the browser, or
/// on another machine.
pub async fn apply_api_key(state: &AppState, id: i64) -> R<Value> {
    let v = reveal_api_key(state, id).await?;
    let key = v["key"].as_str().ok_or("the server returned no key")?.to_string();
    let refreshed = adopt(state, &key).await?;
    Ok(json!({ "id": id, "refreshed_tools": refreshed }))
}

/// Adopt whatever key the account already calls its default.
///
/// This is what makes "the default key is the one buying uses" true on a
/// machine that has never held a key — a second laptop, a reinstall, a fresh
/// data directory. Without it every such machine mints one of its own, and an
/// account that has been installed four times owns four keys it never asked
/// for and cannot tell apart.
///
/// `Ok(None)` — never an error — when there is no usable default, or when the
/// default predates the sealed copy and cannot be read back. Both mean "mint
/// one instead", which is the caller's fallback anyway.
pub(crate) async fn adopt_default(state: &AppState) -> R<Option<String>> {
    let v = authed(state, reqwest::Method::GET, "/api/v1/apikeys", None).await?;
    let row = v["keys"].as_array().and_then(|keys| {
        keys.iter().find(|k| {
            k["is_default"].as_bool().unwrap_or(false)
                && k["usable"].as_bool().unwrap_or(false)
                && k["revealable"].as_bool().unwrap_or(false)
        })
    });
    let Some(id) = row.and_then(|r| r["id"].as_i64()) else {
        return Ok(None);
    };
    let revealed = reveal_api_key(state, id).await?;
    let Some(key) = revealed["key"].as_str().map(String::from) else {
        return Ok(None);
    };
    adopt(state, &key).await?;
    Ok(Some(key))
}

/// Cache a key, load it into the running proxy, and rewrite every buying tool's
/// config to match. Returns the tools that were touched.
async fn adopt(state: &AppState, key: &str) -> R<Vec<String>> {
    super::wallet::remember_key(state, key).await?;
    super::buy::refresh_buy_tool_keys(state, key).await
}

/// The masked form of one of the caller's keys, or `None` if it cannot be read.
/// Best-effort: it only decides whether to drop a local cache.
async fn key_preview_of(state: &AppState, id: i64) -> Option<String> {
    let v = authed(state, reqwest::Method::GET, "/api/v1/apikeys", None).await.ok()?;
    v["keys"]
        .as_array()?
        .iter()
        .find(|k| k["id"].as_i64() == Some(id))?["key_preview"]
        .as_str()
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_matches_the_servers_shape() {
        assert_eq!(preview("sk-asale-E1T3aaaaaaaanwo4"), "sk-asale-E1T3••••nwo4");
        // Short enough that head and tail would overlap: mask the lot rather
        // than print the key back with four bullets in the middle of it.
        assert_eq!(preview("sk-asale-abcd"), "sk-asale-••••");
        // A value that is not one of ours is still masked, never echoed.
        assert!(!preview("some-other-key-entirely").contains("entirely"));
    }
}
