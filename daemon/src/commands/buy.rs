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

/// Re-apply the config of every tool whose buy switch is on but whose live
/// config no longer points at the proxy. Returns the tools that were repaired.
///
/// Drift is ordinary on a real machine: another config switcher took the file
/// over, the user edited it, an upgrade rewrote it, or the proxy port moved
/// between builds. The switch still says "buy", so the config saying otherwise
/// is a state the daemon knows how to fix — asking the user to turn the switch
/// off and on again makes them do by hand what re-applying does exactly. The
/// original stays safe: `set_buy_tool` keeps the backup taken when the switch
/// first went on, so turning it off still restores the user's own file rather
/// than asale's writing.
///
/// Only the *cached* consumer key is used. Minting one needs a live session,
/// and this runs on every Buy-page load — a reconcile that hit the network
/// would fail precisely when the user is signed out, which is when a config is
/// most likely to be found drifted.
/// Tools whose repair has already been reported as failing. The Buy page and
/// the dashboard both poll `buy_tools` (every few seconds), so a config that
/// cannot be written — read-only file, no cached key — would otherwise log the
/// same warning forever. Cleared when the tool repairs, so a recurrence is
/// reported again.
static REPAIR_WARNED: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Report a failed repair the first time, and stay quiet until it succeeds.
fn warn_once(tool: &str, msg: String) {
    let mut warned = REPAIR_WARNED.lock().unwrap_or_else(|e| e.into_inner());
    if warned.insert(tool.to_string()) {
        tracing::warn!(tool, "{msg}");
    }
}

pub async fn reconcile_configs(state: &AppState) -> Vec<String> {
    let key = match super::wallet::cached_key(state).await {
        Ok(Some(k)) => k,
        // No key to write: nothing to re-apply with. The UI still shows the
        // drift warning, which is the honest answer here.
        _ => return Vec::new(),
    };
    let base = tool_config::proxy_base();
    let mut repaired = Vec::new();
    for tool in tool_config::TOOLS {
        let Ok(buy) = state.store.buy_tool(tool).await else { continue };
        if !buy.enabled {
            continue;
        }
        let t = tool.to_string();
        let drifted = tokio::task::spawn_blocking(move || !tool_config::points_at_proxy(&t))
            .await
            .unwrap_or(false);
        if !drifted {
            continue;
        }
        let (t, b, k, models) = (tool.to_string(), base.clone(), key.clone(), buy.models.clone());
        // The snapshot `apply` returns is of asale's own writing (or of whoever
        // took the file over) — dropped on purpose, see `refresh_buy_tool_keys`.
        match tokio::task::spawn_blocking(move || tool_config::apply(&t, &b, &k, &models)).await {
            Ok(Ok(_)) => {
                REPAIR_WARNED.lock().unwrap_or_else(|e| e.into_inner()).remove(*tool);
                repaired.push(tool.to_string());
            }
            Ok(Err(e)) => warn_once(tool, format!("could not re-apply the buy config: {e}")),
            Err(e) => warn_once(tool, format!("re-applying the buy config panicked: {e}")),
        }
    }
    if !repaired.is_empty() {
        tracing::info!("re-applied drifted buy configs: {}", repaired.join(", "));
    }
    repaired
}

/// State of every locally installable AI CLI, for the Buy page: whether it is
/// installed, which subscription account it is signed in as, whether its buy
/// switch is on, which models it may buy, and whether its config actually points
/// at the asale proxy right now.
pub async fn buy_tools(state: &AppState) -> R<Value> {
    let proxy_base = tool_config::proxy_base();
    // Repair before reporting, so a drifted config is fixed by the time the
    // page paints instead of being reported to the user as their problem.
    let repaired = reconcile_configs(state).await;
    // One scan for all three tools — each hits the filesystem/keychain.
    let discovered = tokio::task::spawn_blocking(cli_scan::scan).await.map_err(err)?;

    let mut out = Vec::new();
    for tool in tool_config::TOOLS {
        let t = tool.to_string();
        let (installed, current_base, points_at_proxy) = tokio::task::spawn_blocking(move || {
            (
                tool_config::installed(&t),
                tool_config::current_base_url(&t),
                tool_config::points_at_proxy(&t),
            )
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
        // Each tool addresses it differently — origin, `/v1` root, or its own
        // `/{tool}/v1` prefix — so ask `points_at_proxy`, which owns that per-tool
        // knowledge, rather than restating a subset of it here. Restating it is
        // what made OpenClaw and Hermes report "not in effect" while their configs
        // were in fact pointed at the proxy.
        let in_effect = points_at_proxy;

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
    Ok(json!({ "tools": out, "proxy_base": proxy_base, "repaired": repaired }))
}

/// Which buy-side CLIs are running right now, per tool.
///
/// A buy switch takes effect at the tool's *next* start, so the page has to ask
/// the user to restart it. This is what turns that request from advice into a
/// checkable fact — "two Claude Code sessions are still on the old config, pids
/// 29932 and 38652" — and it is deliberately all this does: see
/// [`crate::proc_scan`] for why there is no button here that restarts anything.
///
/// `scanned: false` means this machine's process table could not be read, which
/// the frontend must not draw as "nothing is running". Every known tool gets a
/// key either way, so the caller never has to distinguish absent from empty.
pub async fn tool_processes() -> R<Value> {
    let found = tokio::task::spawn_blocking(crate::proc_scan::scan).await.map_err(err)?;
    let mut running = serde_json::Map::new();
    for tool in tool_config::TOOLS {
        let list: Vec<&crate::proc_scan::Running> = found
            .iter()
            .flatten()
            .filter(|(t, _)| t == tool)
            .map(|(_, r)| r)
            .collect();
        running.insert(tool.to_string(), serde_json::to_value(list).map_err(err)?);
    }
    Ok(json!({ "scanned": found.is_some(), "running": running }))
}

/// Open one of the buy-side config files in whatever the OS opens it with.
///
/// The Buy page lists the files the switch rewrites; reading one meant copying
/// the path out and finding it by hand, which is exactly the moment a user is
/// already unsure whether asale wrote what it says it wrote.
///
/// The path is matched against the files this daemon itself would write rather
/// than being opened as given: `asaled` can be bound to a non-loopback address,
/// and an RPC that opens any path is a remote file opener for whoever holds the
/// token. Note the file opens on the *daemon's* machine — that is the machine
/// the config lives on, so it is the only answer that means anything.
pub async fn open_config_path(path: String) -> R<Value> {
    let target = buy_config(&path).ok_or_else(|| {
        cmd_err!(
            "errors.tool.notAConfig",
            format!("not a buy config path: {path}"),
            path = path.as_str()
        )
    })?;

    // A listed file need not exist yet: the paths come from `config_paths`,
    // which is what the switch *would* write, and a tool that has never been
    // pointed at the proxy has no file there. Open the folder that holds it
    // instead of failing with "no such file" — the user asked to look, and the
    // folder is where looking continues.
    let dir = !target.is_file();
    let open = if dir {
        target
            .parent()
            .filter(|d| d.is_dir())
            .map(|d| d.to_path_buf())
            .ok_or_else(|| {
                cmd_err!(
                    "errors.tool.configMissing",
                    format!("no config at {path}, and its folder does not exist"),
                    path = path.as_str()
                )
            })?
    } else {
        target
    };

    let shown = open.to_string_lossy().to_string();
    let status = tokio::task::spawn_blocking(move || opener(&open).status())
        .await
        .map_err(err)?
        .map_err(|e| {
            cmd_err!("errors.tool.openFailed", format!("could not open {shown}: {e}"))
        })?;
    if !status.success() {
        // The launcher ran and refused: no handler for a bare `.env`, or no
        // desktop session at all on a headless box. The exit code is all we
        // have — see `opener` for why its output is not captured.
        return Err(cmd_err!(
            "errors.tool.openFailed",
            format!("the system opener exited with {status}")
        ));
    }
    Ok(json!({ "path": shown, "folder": dir }))
}

/// The config file this path names, if it is one the buy switch writes.
///
/// Compared as the frontend received it — `config_paths` is what fills the
/// chips on the Buy page, so a match is a path this daemon itself produced, and
/// no normalisation of the caller's string can widen the set.
fn buy_config(path: &str) -> Option<std::path::PathBuf> {
    tool_config::TOOLS
        .iter()
        .flat_map(|t| tool_config::config_paths(t))
        .find(|p| p.to_string_lossy() == path)
}

/// The platform's "open this with the default handler" launcher.
fn opener(path: &std::path::Path) -> std::process::Command {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // `start` is a `cmd` builtin, and its first quoted argument is the
        // window title — without the empty one a quoted path is taken as the
        // title and nothing opens.
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]).arg(path);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    // The launcher exits as soon as it has handed the file over, but the app it
    // starts is a child of this daemon: leave it nothing of ours to inherit,
    // and never a pipe — an editor that holds one open for its whole lifetime
    // would hang any read of it.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd
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
    // Nothing cached. Take the account's default key before inventing a new one:
    // that is what makes it the *default*, and it is why a second machine does
    // not silently add a fifth key to the owner's list. Best-effort — a server
    // that cannot answer falls through to minting, which is where this used to
    // go unconditionally.
    match super::apikeys::adopt_default(state).await {
        Ok(Some(k)) => return Ok(k),
        Ok(None) => {}
        Err(e) => tracing::debug!("could not adopt the account's default api key: {e}"),
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

/// The models the platform features, with their current price, 24h change and
/// curve — the overview's price ticker (public endpoint).
///
/// Proxied whole rather than assembled here: the server already batches the
/// catalog lookup and the history read into one cached response, so the desktop
/// app makes the same single request the landing page does, and the two agree
/// on which models are featured without the client holding its own list.
pub async fn market_featured(state: &AppState) -> R<Value> {
    let http = asale_client_core::http::plain();
    let resp = http
        .get(format!("{}/api/v1/market/featured", state.cfg.server_api_base))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The path whitelist is the whole of this command's safety: `asaled` can
    /// be bound to a non-loopback address, so whatever it agrees to open is
    /// openable by anyone holding the token. Nothing here launches anything —
    /// the gate is tested, not the platform's opener.
    #[test]
    fn only_the_configs_the_buy_switch_writes_are_openable() {
        for tool in tool_config::TOOLS {
            for p in tool_config::config_paths(tool) {
                let shown = p.to_string_lossy().to_string();
                assert_eq!(buy_config(&shown), Some(p), "{tool} chip is not openable");
            }
        }
        assert_eq!(buy_config("/etc/passwd"), None);
        // A path *inside* a tool's directory is still not one of its files.
        let sneaky = tool_config::tool_dir("claude").join("../../.ssh/id_rsa");
        assert_eq!(buy_config(&sneaky.to_string_lossy()), None);
    }

    /// Every tool answers, whatever the machine looks like. The frontend reads
    /// `running[tool]` straight off the object, so a tool missing here would
    /// read as "nothing running" on a page whose whole point is the difference
    /// between that and "we could not look".
    #[tokio::test]
    async fn the_process_scan_answers_for_every_tool() {
        let v = tool_processes().await.expect("scan");
        assert!(v["scanned"].is_boolean());
        for tool in tool_config::TOOLS {
            assert!(v["running"][tool].is_array(), "{tool} missing from the scan");
        }
    }
}
