//! Buy side: pointing a locally installed AI CLI at the asale proxy and
//! restoring it, plus the market model list the picker shows.

use crate::cli_scan;
use crate::keychain;
use crate::state::AppState;
use crate::tool_config;
use serde_json::{json, Value};
use super::server_client::{authed, resp_json};
use super::{R, err, now_secs};
use crate::cmd_err;

/// The models a tool is allowed to buy. Empty = no restriction. Takes the store
/// rather than the whole app state so the local proxy can call it too.
pub async fn buy_models(store: &asale_client_core::store::LocalStore, tool: &str) -> Vec<String> {
    store.buy_tool(tool).await.map(|r| r.models).unwrap_or_default()
}

/// Is this tool's buy switch on?
pub async fn buy_is_enabled(store: &asale_client_core::store::LocalStore, tool: &str) -> bool {
    store.buy_tool(tool).await.map(|r| r.enabled).unwrap_or(false)
}


/// Carry a pre-existing Claude Code subscription (the older Claude-only
/// `subscribe_claude` flow) over to the per-tool buy switch. Without this an
/// upgrade would leave `~/.claude/settings.json` still pointed at the proxy
/// while `buy_enabled:claude` reads off, so the proxy would refuse the user's
/// own traffic. Runs once at daemon startup and is idempotent.
pub async fn migrate_legacy_subscription(state: &AppState) -> R<bool> {
    if state.store.get_setting("cc_sub_active").await.map_err(err)?.as_deref() != Some("1") {
        return Ok(false);
    }
    // Re-express the old single-file backup in the new multi-file shape so
    // turning the switch off still restores the original verbatim.
    let existed = state.store.get_setting("cc_claude_existed").await.map_err(err)?.as_deref() == Some("1");
    let raw = state.store.get_setting("cc_claude_backup").await.map_err(err)?.filter(|s| !s.is_empty());
    let backup = tool_config::Backup {
        tool: "claude".into(),
        files: vec![tool_config::FileBackup {
            path: tool_config::primary_config_path("claude").to_string_lossy().to_string(),
            raw: if existed { raw } else { None },
        }],
    };
    // The old flow stored a single model; seed the multi-select with it.
    let models: Vec<String> = state
        .store
        .get_setting("cc_sub_model")
        .await
        .map_err(err)?
        .filter(|m| !m.is_empty())
        .into_iter()
        .collect();
    let since: i64 = state
        .store
        .get_setting("cc_sub_since")
        .await
        .map_err(err)?
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(now_secs);
    state
        .store
        .set_buy_tool(
            "claude",
            Some(true),
            Some(&models),
            Some(&serde_json::to_string(&backup).map_err(err)?),
            Some(since),
        )
        .await
        .map_err(err)?;
    // Mark migrated so this never runs twice.
    state.store.set_setting("cc_sub_active", "migrated").await.map_err(err)?;
    tracing::info!("migrated legacy Claude Code subscription to the per-tool buy switch");
    Ok(true)
}

/// State of every locally installable AI CLI, for the Buy page: whether it is
/// installed, which subscription account it is signed in as, whether its buy
/// switch is on, which models it may buy, and whether its config actually points
/// at the asale proxy right now.
pub async fn buy_tools(state: &AppState) -> R<Value> {
    let proxy_base = tool_config::proxy_base();
    // One scan for all three tools — each hits the filesystem/keychain.
    let discovered = tokio::task::spawn_blocking(cli_scan::scan).await.map_err(err)?;

    let mut out = Vec::new();
    for tool in tool_config::TOOLS {
        let t = tool.to_string();
        let (installed, current_base) = tokio::task::spawn_blocking(move || {
            (tool_config::installed(&t), tool_config::current_base_url(&t))
        })
        .await
        .map_err(err)?;

        // The account this CLI is locally signed in as (identity only — the buy
        // side never imports or uses these credentials).
        let account = discovered
            .iter()
            .find(|d| d.provider == *tool)
            .and_then(|d| d.account_hint.clone());
        let plan = discovered.iter().find(|d| d.provider == *tool).and_then(|d| d.plan.clone());

        // One row per tool now, so the switch, the model list and the "buying
        // since" date come back together instead of as three separate lookups.
        let buy = state.store.buy_tool(tool).await.map_err(err)?;
        // "In effect" iff the tool's live config really points at our proxy.
        // Codex addresses the proxy's /v1 root, the others its origin.
        let in_effect = current_base
            .as_deref()
            .is_some_and(|b| b == proxy_base || b == format!("{proxy_base}/v1"));

        out.push(json!({
            "id": tool,
            "label": tool_config::label(tool),
            "installed": installed,
            "account": account,
            "plan": plan,
            "config_path": tool_config::primary_config_path(tool).to_string_lossy(),
            "config_paths": tool_config::config_paths(tool)
                .iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "current_base_url": current_base,
            "enabled": buy.enabled,
            "in_effect": in_effect,
            "models": buy.models,
            "since": buy.since_ts,
        }));
    }
    Ok(json!({ "tools": out, "proxy_base": proxy_base }))
}

/// Turn a tool's buy switch on or off, and set which models it may buy.
///
/// On  → mint/reuse the asale consumer key, rewrite the tool's config to point
///       at the local proxy, and stash the untouched originals for restore.
/// Off → restore those originals byte-for-byte.
///
/// `models` is a multi-select: an empty list means "any model the market
/// offers"; a non-empty list is enforced by the local proxy, which rejects a
/// request for a model this tool is not signed up to buy.
pub async fn set_buy_tool(
    state: &AppState,
    tool: String,
    enabled: bool,
    models: Option<Vec<String>>,
) -> R<Value> {
    if !tool_config::known(&tool) {
        return Err(cmd_err!(
            "errors.tool.unknown",
            // Listed from `TOOLS` rather than spelled out: the hardcoded list
            // went stale the first time a tool was added, and the message then
            // denies the existence of a tool the app is showing a switch for.
            format!("unknown tool: {tool} ({})", tool_config::TOOLS.join(" | ")),
            tool = tool.as_str()
        ));
    }
    // Model selection is stored whether or not the switch changes, so the user
    // can edit it while buying is already on.
    if let Some(models) = &models {
        state
            .store
            .set_buy_tool(&tool, None, Some(models), None, None)
            .await
            .map_err(err)?;
    }

    if enabled {
        // Verify login state (flow §3) before rewriting anything on disk.
        if keychain::get("access_token").map_err(err)?.is_none() {
            return Err(cmd_err!("errors.session.signInToBuy", "sign in before buying"));
        }
        let key = ensure_consumer_key(state).await?;
        let base = tool_config::proxy_base();

        // Editing the model selection of a tool that is already buying
        // re-applies its config, so the tool picks the new model up.
        let prev = state.store.buy_tool(&tool).await.map_err(err)?;
        let selected = prev.models.clone();

        let (t, b, k) = (tool.clone(), base.clone(), key);
        let backup = tokio::task::spawn_blocking(move || tool_config::apply(&t, &b, &k, &selected))
            .await
            .map_err(err)?
            .map_err(err)?;

        // Re-applying snapshots a config asale has already rewritten, so the
        // stored backup must keep the one taken when the switch went on —
        // otherwise turning it off would "restore" asale's own settings.
        let keep_existing = prev.enabled && prev.backup_json.is_some();
        let backup_json = if keep_existing {
            None
        } else {
            Some(serde_json::to_string(&backup).map_err(err)?)
        };
        // "Buying since" dates the switch, not the last model change.
        let since = (!prev.enabled).then(now_secs);
        state
            .store
            .set_buy_tool(&tool, Some(true), None, backup_json.as_deref(), since)
            .await
            .map_err(err)?;
        // Route the proxy through the market so requests reach the asale gateway.
        state.store.set_setting("consume_mode", "market").await.map_err(err)?;

        // This tool's synced account stops being sellable the moment it starts
        // buying. Re-derived here rather than at the next scan, so the pool,
        // the manifest directory and the market session all drop it now.
        super::sell::accounts_changed(state).await;

        Ok(json!({
            "tool": tool,
            "enabled": true,
            "base_url": base,
            "config_paths": backup.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
            "backed_up": backup.had_existing(),
            "models": prev.models,
        }))
    } else {
        let backup: Option<tool_config::Backup> = state
            .store
            .buy_tool(&tool)
            .await
            .map_err(err)?
            .backup_json
            .and_then(|s| serde_json::from_str(&s).ok());

        let t = tool.clone();
        tokio::task::spawn_blocking(move || match backup {
            Some(b) => tool_config::restore(&t, &b),
            // No usable backup (older build, or the row was lost): fall back to
            // stripping just the keys asale injected, leaving the user's own.
            None => tool_config::strip_all(&t),
        })
        .await
        .map_err(err)?
        .map_err(err)?;

        // Switch off and drop the consumed backup in one write.
        state
            .store
            .set_buy_tool(&tool, Some(false), None, Some(""), Some(0))
            .await
            .map_err(err)?;

        // The tool is back on its own credential, so that credential is this
        // device's to sell again — with the switch and cap it was hidden with,
        // since the account row was never removed. Import too, in case the
        // switch went on before this tool was ever scanned. Best-effort: a tool
        // with no local login simply has nothing to bring back.
        let accounts = match super::accounts::import_from_cli(state, tool.clone()).await {
            Ok(v) => v["accounts"].clone(),
            Err(e) => {
                tracing::debug!(tool, "nothing to import after buying stopped: {e}");
                super::sell::accounts_changed(state).await;
                json!([])
            }
        };

        Ok(json!({ "tool": tool, "enabled": false, "restored": true, "accounts": accounts }))
    }
}

/// Resolve (or mint) the consumer API key a bought-through tool presents to the
/// local proxy, making sure the running proxy has it loaded.
pub(crate) async fn ensure_consumer_key(state: &AppState) -> R<String> {
    if let Some(k) = super::wallet::cached_key(state).await? {
        *state.asale_key.write().await = Some(k.clone());
        return Ok(k);
    }
    mint_consumer_key(state).await
}

/// Mint a consumer API key unconditionally, discarding whatever was cached.
///
/// The proxy's self-heal path calls this when the gateway answers a market
/// request with `unknown api key`: the cached key names a key row the server no
/// longer has (the account's keys were revoked, or the deployment's database
/// was rebuilt), and no amount of retrying the same key recovers from that.
pub(crate) async fn mint_consumer_key(state: &AppState) -> R<String> {
    let v = authed(state, reqwest::Method::POST, "/api/v1/apikeys", Some(json!({"label": "asale-buy"}))).await?;
    let k = v["key"].as_str().ok_or("failed to create api key")?.to_string();
    super::wallet::remember_key(state, &k).await?;
    refresh_buy_tool_keys(state, &k).await?;
    Ok(k)
}

/// Write a new consumer key into every tool whose buy switch is on, and report
/// which ones were touched.
///
/// Minting a key does not only produce a new one — it invalidates the old one,
/// and that old one is what is sitting in `~/.claude/settings.json`,
/// `~/.codex/auth.json` and `~/.gemini/.env` right now. Left alone, every tool
/// pointed at the proxy would start answering `401 unknown api key` the moment
/// the key is regenerated, which reads as "asale broke" rather than "the key
/// you replaced is the key they were holding".
pub async fn refresh_buy_tool_keys(state: &AppState, key: &str) -> R<Vec<String>> {
    let base = tool_config::proxy_base();
    let mut refreshed = Vec::new();
    for tool in tool_config::TOOLS {
        let buy = state.store.buy_tool(tool).await.map_err(err)?;
        if !buy.enabled {
            continue;
        }
        let (t, b, k, models) = (tool.to_string(), base.clone(), key.to_string(), buy.models.clone());
        // `apply` returns a snapshot of what it overwrote, which here is
        // asale's own writing — dropped on purpose. The backup a restore must
        // use is the one taken when the switch went on.
        tokio::task::spawn_blocking(move || tool_config::apply(&t, &b, &k, &models))
            .await
            .map_err(err)?
            .map_err(err)?;
        refreshed.push(tool.to_string());
    }
    if !refreshed.is_empty() {
        tracing::info!("re-keyed buying tools after an api key change: {}", refreshed.join(", "));
    }
    Ok(refreshed)
}

/// Market models available to subscribe to (public endpoint on the web API).
pub async fn market_models(state: &AppState) -> R<Value> {
    let http = asale_client_core::http::plain();
    let resp = http
        .get(format!("{}/api/v1/market/models", state.cfg.server_api_base))
        .send()
        .await
        .map_err(err)?;
    resp_json(resp).await
}

/// The 24h window the world map is drawn from — the same one the landing page
/// asks for, so both maps show the same network.
const GLOBE_MINUTES: i64 = 1440;

/// Country membership + seller→buyer token lanes for the overview map (public
/// endpoint, aggregated country-level; it identifies no account).
pub async fn market_globe(state: &AppState) -> R<Value> {
    let http = asale_client_core::http::plain();
    let resp = http
        .get(format!(
            "{}/api/v1/market/globe?minutes={GLOBE_MINUTES}",
            state.cfg.server_api_base
        ))
        .send()
        .await
        .map_err(err)?;
    resp_json(resp).await
}
