//! Subscription accounts: discovering and importing the credentials of the
//! AI CLIs installed on this machine, and the per-account sell switch, daily
//! cap and lane controls built on top of them.

use crate::cli_scan;
use crate::keychain;
use crate::publisher;
use crate::state::AppState;
use asale_client_core::{cli_import, discovery, Provider};
use serde_json::{json, Value};
use super::sell::{accounts_changed};
use super::{R, err, now_secs};
use super::usage::{WINDOW_SECS};

/// Scan the machine for credentials of installed vendor CLIs (Claude Code,
/// Codex, gemini-cli). Returns discoverable sources only — nothing is imported.
pub async fn discovery_scan() -> R<Value> {
    // Filesystem + `security`(1) reads are blocking.
    let found = tokio::task::spawn_blocking(cli_scan::scan).await.map_err(err)?;
    Ok(json!(found))
}

/// Import every CLI credential found on this machine (spec §3.3). Runs
/// automatically at daemon startup and behind the UI's refresh button, so the
/// user never has to trigger a scan by hand. Best-effort: a provider that fails
/// to import is reported, not fatal.
pub async fn import_cli_all(state: &AppState) -> R<Value> {
    let found = tokio::task::spawn_blocking(cli_scan::scan).await.map_err(err)?;

    // A provider can be discovered through several sources (keychain + file);
    // `import_from_cli` merges them per subscription account, so drive it once
    // per provider and let it decide how many accounts that really is.
    let mut providers: Vec<String> = Vec::new();
    for f in &found {
        if !providers.contains(&f.provider) {
            providers.push(f.provider.clone());
        }
    }

    let mut imported = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut errors = Vec::new();
    for provider in providers {
        match import_from_cli(state, provider.clone()).await {
            Ok(v) => {
                if let Some(ws) = v["warnings"].as_array() {
                    for w in ws {
                        if let Some(s) = w.as_str() {
                            if !warnings.iter().any(|x| x == s) {
                                warnings.push(s.to_string());
                            }
                        }
                    }
                }
                // One entry per *account*, not per provider or per file.
                if let Some(accounts) = v["accounts"].as_array() {
                    imported.extend(accounts.iter().cloned());
                }
            }
            Err(e) => {
                tracing::warn!(provider, "cli auto-import failed: {e}");
                errors.push(json!({"provider": provider, "error": e}));
            }
        }
    }

    Ok(json!({"imported": imported, "warnings": warnings, "errors": errors}))
}

/// The already-imported account whose stored refresh token *is* this
/// credential's — i.e. the same login, discovered again. Matching on the token
/// keeps an account's identity stable when the network (and so the profile
/// endpoint) is unavailable, instead of splitting it into a second record under
/// a fallback id.
pub(crate) async fn known_account_for(state: &AppState, provider: &str, cred: &cli_import::CliCred) -> Option<String> {
    let refresh = cred.refresh_token.as_deref().filter(|r| !r.is_empty())?;
    let tools = state.store.list_tools().await.ok()?;
    // The `<provider>-cli` placeholder is deliberately *not* a match: a stale
    // row filed under it holds this very token, and reusing its id would keep
    // resurrecting the nameless duplicate instead of resolving the account's
    // real identity.
    let placeholder = format!("{provider}-cli");
    for tool in tools.iter().filter(|t| t.provider == provider && t.account_id != placeholder) {
        let stored = keychain::get(&keychain::refresh_ref(provider, &tool.account_id)).ok().flatten();
        if stored.as_deref() == Some(refresh) {
            return Some(tool.account_id.clone());
        }
    }
    None
}

/// Resolve the subscription account a credential belongs to. This is the key
/// that decides whether two discovered credentials are one record or two, so it
/// must never collapse different logins — and must not invent a new identity
/// for a login already on file:
///
///   1. the identity the credential itself carries (Codex/Gemini JWT claims);
///   2. an already-imported account holding this exact refresh token;
///   3. for Claude — whose credential stores carry no identity — the OAuth
///      profile endpoint, then the locally signed-in account in `~/.claude.json`;
///   4. otherwise a digest of the refresh token: stable across refreshes, and
///      distinct per login (unlike the old `"<provider>-cli"` placeholder,
///      which merged two unrelated accounts into one row).
pub(crate) async fn resolve_account_id(state: &AppState, provider: &str, cred: &cli_import::CliCred) -> String {
    if let Some(hint) = cred.account_hint.clone().filter(|h| !h.is_empty()) {
        return hint;
    }
    if let Some(known) = known_account_for(state, provider, cred).await {
        return known;
    }
    if provider == "claude" {
        if let Some(email) = cli_scan::claude_profile_email(&cred.access_token).await {
            return email;
        }
        if let Some(email) = tokio::task::spawn_blocking(cli_scan::claude_local_account).await.ok().flatten() {
            return email;
        }
    }
    format!("{provider}-{}", cli_import::token_fingerprint(cred))
}

/// Import the given CLI's credentials into the asale store: tokens go to the
/// encrypted secret store, the SQLite store keeps only the reference (spec
/// §3.4).
///
/// Every store the CLI might use is read, each credential is resolved to its
/// subscription account, and credentials that resolve to the *same* account
/// become **one** record — selling is per subscription account, so a token
/// found in both the keychain and the credential file must not appear twice.
/// The merged row keeps the freshest credential and lists every source it was
/// found in. Returns env-var conflict warnings alongside the accounts.
pub async fn import_from_cli(state: &AppState, provider: String) -> R<Value> {
    if !matches!(provider.as_str(), "claude" | "codex" | "gemini") {
        return Err("unknown CLI provider (claude | codex | gemini)".to_string());
    }
    let prov = provider.clone();
    let candidates = tokio::task::spawn_blocking(move || cli_scan::load_all(&prov)).await.map_err(err)?;
    if candidates.is_empty() {
        return Err(format!("no {provider} CLI credentials found on this machine"));
    }

    // Identity first, merge second: two stores holding one login collapse here.
    let mut identified = Vec::with_capacity(candidates.len());
    for sc in candidates {
        let account_id = resolve_account_id(state, &provider, &sc.cred).await;
        identified.push((account_id, sc));
    }
    let accounts = cli_import::merge_by_account(identified, now_secs());

    let mut out = Vec::with_capacity(accounts.len());
    for acct in accounts {
        let (account_id, cred) = (acct.account_id, acct.cred);
        // Persist exactly like oauth_login: secret-store tokens + store references.
        keychain::set(&keychain::token_ref(&provider, &account_id), &cred.access_token).map_err(err)?;
        if let Some(refresh) = &cred.refresh_token {
            keychain::set(&keychain::refresh_ref(&provider, &account_id), refresh).map_err(err)?;
        }
        if let Some(exp) = cred.expires_at {
            state
                .store
                .set_setting(&publisher::exp_key(&provider, &account_id), &exp.to_string())
                .await
                .map_err(err)?;
        }
        if let Some(plan) = &cred.plan {
            let _ = state.store.set_setting(&format!("plan:{provider}:{account_id}"), plan).await;
        }
        // origin=import: this credential is a *copy* of one the locally installed
        // CLI is also using. asale never writes back to the CLI's own store, but
        // the upstream refresh token is shared, so a refresh by either side can
        // rotate the other out — surfaced in the UI as a warning on the sell
        // switch. The chosen source leads the list; the rest are the other
        // stores that hold this same account.
        let mut sources: Vec<&str> = vec![acct.source.as_str()];
        sources.extend(acct.sources.iter().map(String::as_str).filter(|s| *s != acct.source));
        state
            .store
            .upsert_tool(&provider, &account_id, &keychain::token_ref(&provider, &account_id), &sources, "import")
            .await
            .map_err(err)?;
    
        out.push(json!({
            "provider": provider,
            "account_id": account_id,
            "source": acct.source,
            "sources": sources,
            "plan": cred.plan,
            "expires_at": cred.expires_at,
            "has_refresh_token": cred.refresh_token.is_some(),
        }));
    }
    let dropped = drop_legacy_placeholder(state, &provider, &out).await;
    accounts_changed(state).await;

    // Env conflict detection (spec §3.3): set variables override CLI auth.
    let warnings = cli_import::env_conflicts(&provider, |k| std::env::var(k).ok());

    Ok(json!({"provider": provider, "accounts": out, "dropped": dropped, "warnings": warnings}))
}

/// Retire the pre-identity placeholder row.
///
/// Earlier builds filed a credential whose account they couldn't resolve under
/// the fixed id `<provider>-cli`, so one subscription could end up listed
/// twice — once as `claude-cli`, once under its real email — and the sell side
/// would treat one account as two sellable records. Once the same subscription
/// has a real identity, the placeholder is a duplicate: drop it, carrying its
/// sell switch over when exactly one account was resolved (an unambiguous
/// rename, where silently switching selling off would be a surprise).
///
/// Returns the account ids removed. Best-effort — never fails the import.
pub(crate) async fn drop_legacy_placeholder(state: &AppState, provider: &str, imported: &[Value]) -> Vec<String> {
    let placeholder = format!("{provider}-cli");
    if imported.iter().any(|a| a["account_id"] == json!(placeholder)) {
        return Vec::new();
    }
    let Ok(tools) = state.store.list_tools().await else { return Vec::new() };
    let Some(stale) = tools.iter().find(|t| t.provider == provider && t.account_id == placeholder) else {
        return Vec::new();
    };
    if stale.sell_enabled && imported.len() == 1 {
        if let Some(heir) = imported[0]["account_id"].as_str() {
            let _ = state.store.set_tool_sell(provider, heir, true, stale.sell_daily_limit).await;
        }
    }
    tracing::info!(provider, "retiring legacy `{placeholder}` account row — the subscription now has a real identity");
    match remove_account(state, provider.to_string(), placeholder.clone()).await {
        Ok(_) => vec![placeholder],
        Err(e) => {
            tracing::warn!(provider, "could not retire `{placeholder}`: {e}");
            Vec::new()
        }
    }
}

/// Every connected subscription account, one row each — the Sell page lists
/// these and switches them on/off individually. Each row carries the live pool
/// status plus everything the per-account sell limit UI needs:
///
///   - `sell_enabled` / `sell_daily_limit` — this account's own switch and cap.
///   - `used_today` / `used_window`        — tokens *this account* served, from
///                                           account-attributed metering.
///   - `window_cap` / `daily_cap`          — its plan's 5h cap and the daily
///                                           equivalent (×24/5), so a cap can be
///                                           expressed as a % of the plan.
///   - `origin` / `shared_with_local_cli`  — whether the credential is asale's
///                                           own OAuth login or a copy of the
///                                           one an installed CLI is using.
pub async fn list_accounts(state: &AppState) -> R<Value> {
    // Rebuild first so quota/expiry reflect the store's current truth.
    publisher::rebuild_pool(&state.store, &state.pool).await;
    let statuses = {
        let pool = state.pool.lock().map_err(|_| "pool lock poisoned".to_string())?;
        pool.statuses(now_secs())
    };
    let tools = state.store.list_tools().await.map_err(err)?;

    let mut out = Vec::with_capacity(statuses.len());
    for s in statuses {
        let tool = tools.iter().find(|t| t.provider == s.provider && t.account_id == s.account_id);
        let plan = s.plan.clone();
        let window_cap = Provider::from_str_opt(&s.provider)
            .map(|p| discovery::plan_window_cap(p, plan.as_deref()))
            .unwrap_or(0) as i64;
        let used_window = state
            .store
            .served_tokens_since_for_account(WINDOW_SECS, &s.provider, &s.account_id)
            .await
            .map_err(err)? as i64;
        let mut row = serde_json::to_value(&s).map_err(err)?;
        if let Some(obj) = row.as_object_mut() {
            obj.insert("window_cap".into(), json!(window_cap));
            // Daily equivalent of the 5h rolling cap (24h / 5h = 4.8×).
            obj.insert("daily_cap".into(), json!(window_cap * 24 / 5));
            obj.insert("used_window".into(), json!(used_window));
            obj.insert("source".into(), json!(tool.and_then(|t| t.source.clone())));
            // Every store holding this same account. Two entries mean the
            // keychain and the credential file are one subscription, merged
            // into this single sellable record — not two accounts.
            obj.insert("sources".into(), json!(tool.map(|t| t.sources.clone()).unwrap_or_default()));
            obj.insert(
                "shared_with_local_cli".into(),
                json!(s.origin.as_deref() != Some("oauth")),
            );
        }
        out.push(row);
    }
    Ok(json!(out))
}

/// Turn one account's sell switch on/off and set its daily token cap
/// (0 = unlimited). Selling is per account, never per provider: switching one
/// Claude account on leaves your other Claude accounts untouched.
pub async fn set_account_sell(
    state: &AppState,
    provider: String,
    account_id: String,
    enabled: bool,
    daily_limit: Option<i64>,
) -> R<Value> {
    let tools = state.store.list_tools().await.map_err(err)?;
    let existing = tools
        .iter()
        .find(|t| t.provider == provider && t.account_id == account_id)
        .ok_or("unknown account")?;
    // Omitting dailyLimit keeps the account's current cap (the UI toggles the
    // switch and edits the cap independently).
    let limit = daily_limit.unwrap_or(existing.sell_daily_limit).max(0);

    state
        .store
        .set_tool_sell(&provider, &account_id, enabled, limit)
        .await
        .map_err(err)?;
    accounts_changed(state).await;

    Ok(json!({
        "provider": provider,
        "account_id": account_id,
        "sell_enabled": enabled,
        "sell_daily_limit": limit,
    }))
}

/// Every `(account, model)` lane and why it is or is not selling (spec §4.5).
///
/// This is what the sell page renders. An account row alone cannot answer "why
/// is Opus earning nothing while Haiku is fine?" — that difference lives here.
pub async fn list_lanes(state: &AppState) -> R<Value> {
    publisher::rebuild_pool(&state.store, &state.pool).await;
    let views = {
        let pool = state.pool.lock().map_err(|_| "pool lock poisoned".to_string())?;
        pool.lane_views(now_secs())
    };
    Ok(json!({ "lanes": views }))
}

/// Resume selling a lane the operator was asked to fix.
///
/// Three things have to happen together, and skipping any one of them leaves
/// the lane looking resumed while still earning nothing: clear the local pause,
/// forget the persisted one (or a restart re-pauses it), and tell the gateway —
/// which keeps a repeatedly-failing lane out across ordinary re-declarations
/// and only lets it back on an explicit `supply.resume`.
///
/// An empty `model` resumes every paused lane of the account; an empty
/// `provider` resumes everything.
pub async fn resume_lane(
    state: &AppState,
    provider: String,
    account_id: String,
    model: String,
) -> R<Value> {
    {
        let mut pool = state.pool.lock().map_err(|_| "pool lock poisoned".to_string())?;
        if provider.is_empty() || account_id.is_empty() || model.is_empty() {
            pool.resume_all();
        } else {
            pool.resume_lane(&provider, &account_id, &model);
        }
    }
    state
        .store
        .clear_lane_pause(&provider, &account_id, &model)
        .await
        .map_err(err)?;
    if let Some(h) = state.publisher.read().await.as_ref() {
        h.resume(&model);
    }
    Ok(json!({"resumed": true, "provider": provider, "account_id": account_id, "model": model}))
}

/// Remove an imported account: secret-store entries, store row, auth manifest,
/// pool slot.
pub async fn remove_account(state: &AppState, provider: String, account_id: String) -> R<bool> {
    let removed = state.store.delete_tool(&provider, &account_id).await.map_err(err)?;
    keychain::delete(&keychain::token_ref(&provider, &account_id)).map_err(err)?;
    keychain::delete(&keychain::refresh_ref(&provider, &account_id)).map_err(err)?;
    let _ = state.store.set_setting(&publisher::exp_key(&provider, &account_id), "").await;
    let _ = state.store.set_setting(&format!("plan:{provider}:{account_id}"), "").await;
    // The manifest file goes with it: `accounts_changed` rewrites the whole
    // directory from the table, so a removed account leaves nothing behind.
    accounts_changed(state).await;
    Ok(removed)
}
