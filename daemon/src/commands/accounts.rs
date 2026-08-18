//! Subscription accounts: discovering and importing the credentials of the
//! AI CLIs installed on this machine, and the per-account sell switch, daily
//! cap and lane controls built on top of them.

use crate::cli_scan;
use crate::keychain;
use crate::publisher;
use crate::state::AppState;
use asale_client_core::{cli_import, discovery, Provider, Wire};
use serde_json::{json, Value};
use super::sell::{accounts_changed, credential_replaced};
use super::{R, err, now_secs};
use super::usage::{WINDOW_SECS};
use crate::cmd_err;

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
    let mut skipped: Vec<Value> = Vec::new();
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
                // Left out because this tool is buying — reported, not an
                // error, so the UI can say why an account it expected is
                // missing instead of leaving the user to guess.
                if v["skipped"] == json!("buying") {
                    skipped.push(json!({
                        "provider": provider,
                        "label": crate::tool_config::label(&provider),
                        "reason": "buying",
                        "dropped": v["dropped"].clone(),
                    }));
                }
            }
            Err(e) => {
                tracing::warn!(provider, "cli auto-import failed: {e}");
                errors.push(json!({"provider": provider, "error": e.message}));
            }
        }
    }

    Ok(json!({"imported": imported, "warnings": warnings, "skipped": skipped, "errors": errors}))
}

/// Accounts of `provider` whose credential asale holds exclusively — its own
/// browser login or a pasted key — rather than a copy of a local CLI's.
async fn exclusively_held_accounts(state: &AppState, provider: &str) -> std::collections::HashSet<String> {
    state
        .store
        .list_tools()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.provider == provider && matches!(t.origin.as_deref(), Some("oauth") | Some("api_key")))
        .map(|t| t.account_id)
        .collect()
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
        return Err(cmd_err!("errors.cli.unknownProvider", "unknown CLI provider (claude | codex | gemini)"));
    }
    // A tool that is buying is a consumer of the market, not a supplier — see
    // `tool_config::is_buying`. Checked before the scan, so a credential asale
    // itself wrote into that tool's directory is never even read: with the buy
    // switch on and no ChatGPT login of its own, `~/.codex/auth.json` holds
    // nothing but our own consumer key, and importing it would put that key up
    // for sale.
    if crate::tool_config::is_buying(&state.store, &provider).await {
        return Ok(json!({
            "provider": provider,
            "accounts": [],
            "dropped": [],
            "warnings": [],
            "skipped": "buying",
        }));
    }

    let prov = provider.clone();
    let candidates = tokio::task::spawn_blocking(move || cli_scan::load_all(&prov)).await.map_err(err)?;
    if candidates.is_empty() {
        return Err(cmd_err!(
            "errors.cli.noCredentials",
            format!("no {provider} CLI credentials found on this machine"),
            provider = provider.as_str()
        ));
    }

    // Identity first, merge second: two stores holding one login collapse here.
    let mut identified = Vec::with_capacity(candidates.len());
    for sc in candidates {
        let account_id = resolve_account_id(state, &provider, &sc.cred).await;
        identified.push((account_id, sc));
    }
    let accounts = cli_import::merge_by_account(identified, now_secs());
    let held_exclusively = exclusively_held_accounts(state, &provider).await;

    let mut out = Vec::with_capacity(accounts.len());
    for acct in accounts {
        let (account_id, cred) = (acct.account_id, acct.cred);
        // An account asale logged into itself is not a copy of the CLI's — it
        // has its own token pair. Overwriting it here would quietly put the
        // account back on the CLI's shared refresh token while `origin` (which
        // `upsert_tool` protects, and which drives the shared-rotation warning)
        // keeps saying asale holds it exclusively. The row still learns that a
        // CLI on this machine has the same account, via `sources` below.
        let exclusive = held_exclusively.contains(&account_id);
        // Persist exactly like oauth_login: secret-store tokens + store references.
        if !exclusive {
            keychain::set(&keychain::token_ref(&provider, &account_id), &cred.access_token).map_err(err)?;
            if let Some(refresh) = &cred.refresh_token {
                keychain::set(&keychain::refresh_ref(&provider, &account_id), refresh).map_err(err)?;
            }
            // Belongs to the credential above: keeping the CLI's expiry against
            // asale's own token would refresh it at the wrong moment.
            if let Some(exp) = cred.expires_at {
                state
                    .store
                    .set_setting(&publisher::exp_key(&provider, &account_id), &exp.to_string())
                    .await
                    .map_err(err)?;
            }
        }
        if let Some(plan) = &cred.plan {
            let _ = state.store.set_setting(&format!("plan:{provider}:{account_id}"), plan).await;
        }
        // Codex's upstream refuses the bearer without the account id that goes
        // with it; carrying it out of auth.json here is what keeps a re-import
        // (or a first import) from producing an account that 401s on its first
        // sale and is then marked as needing a fresh login.
        if let Some(up) = &cred.upstream_account_id {
            let _ = state
                .store
                .set_setting(&publisher::upstream_acct_key(&provider, &account_id), up)
                .await;
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
    // Re-importing is how somebody who signed into their CLI again hands the
    // new credential to asale, so it clears the old one's pause for the same
    // reason a fresh OAuth login does.
    for row in &out {
        if let Some(id) = row.get("account_id").and_then(|v| v.as_str()) {
            credential_replaced(state, &provider, id).await;
        }
    }
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

/// Whether this credential is a metered platform key rather than a plan
/// subscription. A key is billed against a balance: it has no rolling window and
/// no plan cap, so every window-derived number the Sell page shows would be a
/// fabrication for it — the UI hides them and leans on the daily sell cap
/// instead. Three ways one gets here: the key-only providers, a key pasted into
/// asale (`origin = api_key`), and a key lifted out of an installed CLI's
/// credential file, which `cli_import` labels with this exact account id.
fn is_key_credential(provider: &str, account_id: &str, origin: Option<&str>) -> bool {
    origin == Some("api_key")
        || account_id == "api-key"
        || Provider::from_str_opt(provider).is_some_and(asale_protocol::ids::is_api_key_provider)
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
///   - `key_based`                         — a metered key, so the window
///                                           numbers above do not apply to it.
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
        // Which of the two answers about this account's headroom is in force.
        // The page has always shown `used_window / window_cap`, which is a
        // local count against a guessed cap — and on a Claude login, where the
        // plan is never known, a cap so low that a live subscription reads as
        // spent. When the provider's own reading is gating the account instead,
        // say so and show *its* number, or the page contradicts the lane it is
        // describing: 100% of a made-up cap, still selling.
        let gate = super::usage::account_quota_gate(&state.store, &s.provider, &s.account_id, now_secs()).await;
        let mut row = serde_json::to_value(&s).map_err(err)?;
        if let Some(obj) = row.as_object_mut() {
            obj.insert("window_cap".into(), json!(window_cap));
            // Daily equivalent of the 5h rolling cap (24h / 5h = 4.8×).
            obj.insert("daily_cap".into(), json!(window_cap * 24 / 5));
            obj.insert("used_window".into(), json!(used_window));
            obj.insert(
                "window_source".into(),
                json!(if gate.is_some() { "upstream" } else { "estimate" }),
            );
            obj.insert("window_used_percent".into(), json!(gate.as_ref().map(|(g, _)| (1.0 - g.headroom) * 100.0)));
            obj.insert("window_as_of".into(), json!(gate.as_ref().map(|(_, at)| *at)));
            obj.insert("window_key".into(), json!(gate.as_ref().map(|(g, _)| g.tightest.clone())));
            obj.insert("source".into(), json!(tool.and_then(|t| t.source.clone())));
            // Every store holding this same account. Two entries mean the
            // keychain and the credential file are one subscription, merged
            // into this single sellable record — not two accounts.
            obj.insert("sources".into(), json!(tool.map(|t| t.sources.clone()).unwrap_or_default()));
            // Only a credential *copied out of* a locally installed CLI is
            // shared with it. asale's own OAuth login is not, and neither is a
            // pasted API key — flagging those would warn about a rotation risk
            // that cannot happen to them.
            obj.insert(
                "shared_with_local_cli".into(),
                json!(s.origin.as_deref() == Some("import")),
            );
            obj.insert(
                "key_based".into(),
                json!(is_key_credential(&s.provider, &s.account_id, s.origin.as_deref())),
            );
        }
        out.push(row);
    }
    Ok(json!(out))
}

/// Turn one account's sell switch on/off and set its selling terms: the daily
/// token cap (0 = unlimited), the market discount band it will sell inside, and
/// how many requests it serves at once. Selling is per account, never per
/// provider: switching one Claude account on leaves your other Claude accounts
/// untouched.
///
/// Every argument past `enabled` is optional and means "leave this as it is" —
/// the UI edits the switch, the cap and the band independently, and a form that
/// only touched one of them must not reset the others.
pub async fn set_account_sell(
    state: &AppState,
    provider: String,
    account_id: String,
    enabled: bool,
    daily_limit: Option<i64>,
    min_ratio: Option<i64>,
    max_ratio: Option<i64>,
    concurrency: Option<i64>,
) -> R<Value> {
    let tools = state.store.list_tools().await.map_err(err)?;
    let existing = tools
        .iter()
        .find(|t| t.provider == provider && t.account_id == account_id)
        .ok_or("unknown account")?;
    // Putting an account on the market needs a session: the switch only means
    // anything once this device can register with the market and declare its
    // lanes. Without one the publisher sits in a reconnect loop logging
    // "session expired" behind a switch that looks on — so refuse here and let
    // the frontend send the user to sign in.
    //
    // Only the off → on transition. Editing the terms of an account that is
    // already selling, and switching one off, are local edits a signed-out user
    // must still be able to make — being unable to *stop* selling because your
    // session lapsed would be the worse failure.
    if enabled && !existing.sell_enabled && keychain::get("access_token").map_err(err)?.is_none() {
        return Err(cmd_err!("errors.session.signInToSell", "sign in before selling"));
    }
    // Omitting dailyLimit keeps the account's current cap (the UI toggles the
    // switch and edits the cap independently).
    let limit = daily_limit.unwrap_or(existing.sell_daily_limit).max(0);
    let (band_lo, band_hi) = asale_client_core::store::normalise_band(
        min_ratio.unwrap_or(existing.sell_min_ratio),
        max_ratio.unwrap_or(existing.sell_max_ratio),
    );
    let lanes = asale_client_core::store::normalise_concurrency(
        concurrency.unwrap_or(existing.sell_concurrency),
    );

    state
        .store
        .set_tool_sell(&provider, &account_id, enabled, limit)
        .await
        .map_err(err)?;
    state
        .store
        .set_tool_ratio_band(&provider, &account_id, band_lo, band_hi)
        .await
        .map_err(err)?;
    state
        .store
        .set_tool_concurrency(&provider, &account_id, lanes)
        .await
        .map_err(err)?;
    // Rebuilds the pool and nudges the live session, which is what re-declares
    // the lane — a concurrency change the market has not been told about would
    // only take effect at the next periodic re-declaration.
    accounts_changed(state).await;

    Ok(json!({
        "provider": provider,
        "account_id": account_id,
        "sell_enabled": enabled,
        "sell_daily_limit": limit,
        "sell_min_ratio": band_lo,
        "sell_max_ratio": band_hi,
        "sell_concurrency": lanes,
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
    // An auth failure pauses every lane of the account, so resuming one clears
    // all of them — and the persisted rows and the gateway have to be told about
    // exactly the same set, or a restart (or the next re-declaration) puts the
    // lanes the pool has already forgiven back out of service.
    let cleared = {
        let mut pool = state.pool.lock().map_err(|_| "pool lock poisoned".to_string())?;
        if provider.is_empty() || account_id.is_empty() || model.is_empty() {
            pool.resume_all();
            Vec::new()
        } else {
            pool.resume_lane(&provider, &account_id, &model)
        }
    };
    if cleared.len() > 1 {
        // Wildcard on the model column: the whole account came back at once.
        state.store.clear_lane_pause(&provider, &account_id, "").await.map_err(err)?;
    } else {
        state.store.clear_lane_pause(&provider, &account_id, &model).await.map_err(err)?;
    }
    if let Some(h) = state.publisher.read().await.as_ref() {
        if cleared.len() > 1 {
            for m in &cleared {
                h.resume(m);
            }
        } else {
            h.resume(&model);
        }
    }
    Ok(json!({"resumed": true, "provider": provider, "account_id": account_id, "model": model}))
}

// ── Custom endpoints ─────────────────────────────────────────────
//
// A custom account is an endpoint the operator supplies: a base URL, a key, and
// whatever models that endpoint serves. It sells through exactly the same path
// as a subscription — same lanes, same price band, same concurrency ceiling,
// same metering — and differs in only two places: the upstream URL travels with
// the account instead of being built by the gateway (see `executor::execute`),
// and the model set comes from the endpoint's own model list rather than from a
// vendor's plan.
//
// It is platform-internal machinery for putting supply behind models the
// subscription sellers happen not to cover, and it is gated on the seller being
// a platform operator — the first difference above is why. An upstream URL that
// travels with the account means the gateway does not decide where a buyer's
// prompt goes; whoever filled in the form does. Every other provider at least
// names a vendor.
//
// The rule is the server's (`wsrelay::session::declare_supply` drops a `custom`
// item from anybody else) and is mirrored here so the sell page does not offer
// a form whose every result would be silently refused. It was briefly open to
// everyone, which is what `purge_custom_endpoints` exists to undo.
//
// `ASALE_CUSTOM_ENDPOINTS=0` is a second, independent gate: a build that should
// not carry the feature at all.

/// Env var that turns the custom-endpoint commands off.
const CUSTOM_ENDPOINTS_ENV: &str = "ASALE_CUSTOM_ENDPOINTS";

/// Whether this build is running the custom-endpoint feature at all.
///
/// On unless explicitly turned off — the gate that decides *who* may use it is
/// [`platform_operator`], not this one. This variable stays as the way to build
/// a client that does not have the commands at all.
pub fn custom_endpoints_enabled() -> bool {
    !matches!(
        std::env::var(CUSTOM_ENDPOINTS_ENV).ok().as_deref().map(str::trim),
        Some("0") | Some("false") | Some("no")
    )
}

/// How long the operator verdict is reused before `/me/profile` is asked again.
///
/// The sell page polls the status below every few seconds, so without a cache
/// an open sell page would be a `/me/profile` call on that same clock. A minute
/// is short enough that a promotion — or a sign-out — still shows up while the
/// user is looking at the page.
const OPERATOR_TTL_SECS: i64 = 60;

/// Whether the signed-in account may sell through a custom endpoint.
///
/// `Some(true)` — a platform operator (`users.role == "admin"`).
/// `Some(false)` — the server answered, and this account is not one.
/// `None` — nobody is signed in, or the server did not answer.
///
/// The third case exists because the two things that read this want opposite
/// defaults from it. Showing the tile wants "no unless told yes": an unknown
/// answer must not offer a feature the gateway will refuse. Deleting somebody's
/// endpoints wants "no unless told no": a flight with no wifi is not a
/// demotion, and the accounts it would take away carry keys the user pasted in
/// by hand.
///
/// A cache miss is served under the write lock, so the sell page's poll cannot
/// have three of these in flight at once while a slow `/me/profile` is pending:
/// whoever arrives second waits for the first and reads its answer.
pub async fn platform_operator(state: &AppState) -> Option<bool> {
    if let Some((at, verdict)) = *state.operator.read().await {
        if now_secs() - at < OPERATOR_TTL_SECS {
            return verdict;
        }
    }
    let mut slot = state.operator.write().await;
    // Somebody may have filled it while we waited for the lock.
    if let Some((at, verdict)) = *slot {
        if now_secs() - at < OPERATOR_TTL_SECS {
            return verdict;
        }
    }
    // A missing or non-string `role` is the server declining to say — see
    // `api::profile::get_profile`, which leaves it null when the lookup fails
    // rather than guessing "user" and having us delete somebody's endpoints.
    let verdict = match super::me_profile(state).await {
        Ok(v) => v["role"].as_str().map(|r| r == "admin"),
        Err(_) => None,
    };
    *slot = Some((now_secs(), verdict));
    verdict
}

/// Whether this daemon will accept the custom-endpoint commands.
///
/// Two gates, and the tile needs both: the build has to be running the feature
/// at all, and the signed-in account has to be a platform operator. The second
/// is the server's rule — `wsrelay::session::declare_supply` drops a `custom`
/// lane from anyone else — mirrored here so an ordinary seller is not offered a
/// form whose every result would be silently refused.
pub async fn custom_endpoints_status(state: &AppState) -> R<Value> {
    let enabled = custom_endpoints_enabled() && platform_operator(state).await == Some(true);
    Ok(json!({"enabled": enabled}))
}

/// Gate a custom-endpoint command on the server's own rule.
///
/// An unknown verdict refuses too: the alternative is connecting an endpoint
/// that the gateway will drop from every declaration, which looks to its
/// operator like a lane that sells nothing for no reason.
async fn require_operator(state: &AppState) -> R<()> {
    match platform_operator(state).await {
        Some(true) => Ok(()),
        _ => Err(cmd_err!(
            "errors.cli.customEndpointsAdminOnly",
            "custom endpoints are available to platform operators only"
        )),
    }
}

/// The account ids of this device's custom endpoints, if it has any.
///
/// A local SQLite read, and on virtually every install it comes back empty —
/// which is what makes it worth doing before anything that costs a request.
async fn custom_endpoint_ids(state: &AppState) -> Vec<String> {
    match state.store.list_tools().await {
        Ok(tools) => tools
            .iter()
            .filter(|t| t.provider == publisher::CUSTOM_PROVIDER)
            .map(|t| t.account_id.clone())
            .collect(),
        Err(e) => {
            tracing::warn!("reading the tool list failed: {e}");
            Vec::new()
        }
    }
}

/// Take this device's custom endpoints away if the account behind them is not a
/// platform operator (any more). Answers how many were removed.
///
/// The supervisor runs this once a minute, forever, on every install — so the
/// order matters: the local "does this device even have one" read comes first,
/// and the `/me/profile` call happens only on the few devices where the answer
/// could change anything. Otherwise every client on the network, signed in or
/// not, would be asking a question about a feature it does not use on the same
/// one-minute clock.
pub async fn enforce_custom_endpoint_policy(state: &AppState) -> usize {
    if custom_endpoint_ids(state).await.is_empty() {
        return 0;
    }
    // Only on a definite `Some(false)`: not signed in, or a server that did not
    // answer, must never be the reason somebody's pasted-in keys are deleted.
    if platform_operator(state).await != Some(false) {
        return 0;
    }
    purge_custom_endpoints(state).await
}

/// Remove every custom endpoint on this device, keys and settings included.
///
/// Called when the server has said this account is not a platform operator.
/// Selling through a custom endpoint used to be open to everyone, so an
/// ordinary seller may well have one connected; leaving it would leave an
/// account on the sell page that is declared every minute and refused every
/// minute, with nothing on the page to say why.
///
/// Returns how many were removed, so the caller can keep quiet when there was
/// nothing to do — which is the usual case, on every device, forever.
pub async fn purge_custom_endpoints(state: &AppState) -> usize {
    let mut removed = 0;
    for account_id in custom_endpoint_ids(state).await {
        // Not `remove_custom_endpoint`: that one refuses when the feature is
        // switched off, and a build with `ASALE_CUSTOM_ENDPOINTS=0` is exactly
        // where a leftover endpoint most needs clearing out.
        match forget_custom_endpoint(state, &account_id).await {
            Ok(_) => {
                removed += 1;
                tracing::info!(account = %account_id, "removed a custom endpoint: this account is not a platform operator");
            }
            Err(e) => tracing::warn!(account = %account_id, "removing a custom endpoint failed: {e}"),
        }
    }
    removed
}

/// Re-read one endpoint's model list now, rather than at the next hourly
/// refresh.
///
/// The list only moves when somebody reconfigures the endpoint — which is
/// exactly when its operator is looking at this page and wondering why the new
/// model is not on offer yet.
pub async fn refresh_custom_endpoint(state: &AppState, account_id: String) -> R<Value> {
    if !custom_endpoints_enabled() {
        return Err(cmd_err!(
            "errors.cli.customEndpointsDisabled",
            "custom endpoints are turned off on this client (ASALE_CUSTOM_ENDPOINTS=0)"
        ));
    }
    require_operator(state).await?;
    let tool = state
        .store
        .list_tools()
        .await
        .map_err(err)?
        .into_iter()
        .find(|t| t.provider == publisher::CUSTOM_PROVIDER && t.account_id == account_id)
        .ok_or("unknown endpoint")?;
    let base = state
        .store
        .get_setting(&publisher::custom_base_key(&account_id))
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let Some(key) = keychain::get(&tool.keychain_ref).ok().flatten() else {
        return Err(cmd_err!("errors.cli.customEndpointNoKey", "this endpoint has no key in the secret store"));
    };
    // Asked in the protocol it was connected under. Re-detecting here would let
    // a re-read quietly change what this account speaks, which is a decision
    // for the connect form, not for a refresh button.
    let wire = publisher::custom_wire(&state.store, &account_id).await;
    let models = discovery::custom_endpoint_models(&base, &key, wire)
        .await
        .map_err(|e| cmd_err!("errors.cli.customEndpointUnreachable", format!("endpoint check failed: {e}")))?;
    publisher::store_custom_models(&state.store, &account_id, &models).await.map_err(err)?;
    // Rebuild + re-declare, so a model that just became available is on the
    // market now instead of at the next periodic declaration.
    accounts_changed(state).await;
    Ok(json!({
        "account_id": account_id,
        "endpoint_models": models.len(),
        "sellable_models": sellable_custom_models(state, &account_id).await,
    }))
}

/// Remove a custom endpoint: the account row, its key, and the settings that
/// only this kind of account has.
///
/// `remove_account` handles the first two for every provider; the base URL, the
/// protocol and the cached model list are this feature's own, and leaving them
/// behind would silently re-arm a re-added endpoint with a stale list — or,
/// worse, with the dialect the *previous* endpoint of that name spoke.
pub async fn remove_custom_endpoint(state: &AppState, account_id: String) -> R<bool> {
    if !custom_endpoints_enabled() {
        return Err(cmd_err!(
            "errors.cli.customEndpointsDisabled",
            "custom endpoints are turned off on this client (ASALE_CUSTOM_ENDPOINTS=0)"
        ));
    }
    // Deliberately not gated on `require_operator`: taking an endpoint away is
    // the one action that stays available to somebody who has lost the right to
    // add one.
    forget_custom_endpoint(state, &account_id).await
}

/// The removal itself, with no gate on it. See [`remove_custom_endpoint`] for
/// what it clears and why.
async fn forget_custom_endpoint(state: &AppState, account_id: &str) -> R<bool> {
    let removed =
        remove_account(state, publisher::CUSTOM_PROVIDER.to_string(), account_id.to_string()).await?;
    let _ = state.store.set_setting(&publisher::custom_base_key(account_id), "").await;
    let _ = state.store.set_setting(&publisher::custom_wire_key(account_id), "").await;
    let _ = publisher::store_custom_models(&state.store, account_id, &[]).await;
    Ok(removed)
}

/// Connect (or reconfigure) a custom OpenAI-compatible endpoint and put it on
/// the market.
///
/// The endpoint is probed before anything is stored: `GET {base}/models` is the
/// one call all four dialects answer, so it verifies the base URL and the key
/// together and returns the model list in the same round trip. A base or key
/// that does not work fails here rather than on the first consumer request,
/// where it would cost a buyer a turn and this device its reputation.
///
/// `wire` is the protocol the endpoint speaks. Omitted, it is *found* — the
/// probe tries each dialect and keeps the first that answers, because the
/// moment of connecting is when its operator is least sure (a reseller's docs
/// say "OpenAI-compatible" for a host that also serves `/messages`) and a wrong
/// answer is only discovered on the first sale.
///
/// `min_ratio` is the floor this endpoint sells at, in whole percent *of* list
/// price — the same number and the same convention as a subscription's price
/// band, because it is the same decision: below this, do not trade. It is what
/// keeps a metered endpoint from selling under what its own tokens cost.
/// `concurrency` is how many requests it serves at once.
///
/// Re-running it for an existing `label` updates that account in place: the
/// endpoint, the key and the terms are all rewritten, and the cached model list
/// is replaced by what the probe just returned.
#[allow(clippy::too_many_arguments)]
pub async fn connect_custom_endpoint(
    state: &AppState,
    base_url: String,
    api_key: String,
    wire: Option<String>,
    label: Option<String>,
    min_ratio: Option<i64>,
    concurrency: Option<i64>,
    enabled: Option<bool>,
) -> R<Value> {
    if !custom_endpoints_enabled() {
        return Err(cmd_err!(
            "errors.cli.customEndpointsDisabled",
            "custom endpoints are turned off on this client (ASALE_CUSTOM_ENDPOINTS=0)"
        ));
    }
    require_operator(state).await?;
    let base = base_url.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return Err(cmd_err!("errors.cli.customBaseEmpty", "base URL is empty"));
    }
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(cmd_err!(
            "errors.cli.customBaseScheme",
            "base URL must start with http:// or https://"
        ));
    }
    let key = api_key.trim().trim_matches(['"', '\'']).trim().to_string();
    if key.is_empty() {
        return Err(cmd_err!("errors.cli.apiKeyEmpty", "API key is empty"));
    }
    if key.chars().any(char::is_whitespace) {
        return Err(cmd_err!(
            "errors.cli.apiKeyHasWhitespace",
            "API key contains whitespace — paste just the key, not the whole command"
        ));
    }

    // A named protocol this build cannot speak is refused rather than quietly
    // demoted to the default: it would connect, sell, and then answer every
    // request in a dialect the endpoint never agreed to.
    let asked = match wire.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
        None => None,
        Some(w) => Some(Wire::from_str_opt(w).ok_or_else(|| {
            cmd_err!(
                "errors.cli.customWireUnknown",
                format!("unknown endpoint protocol `{w}` (openai | claude | gemini | responses)")
            )
        })?),
    };

    // The probe is the connect check: it answers "is this an endpoint of that
    // protocol, does this key work on it, and what does it serve" at once —
    // and, when no protocol was named, which one it is.
    let (wire, models) = match asked {
        Some(w) => {
            let models = discovery::custom_endpoint_models(&base, &key, w).await.map_err(|e| {
                cmd_err!("errors.cli.customEndpointUnreachable", format!("endpoint check failed: {e}"))
            })?;
            (w, models)
        }
        None => discovery::detect_custom_endpoint_wire(&base, &key)
            .await
            .map_err(|e| cmd_err!("errors.cli.customEndpointUnreachable", format!("endpoint check failed: {e}")))?,
    };
    if models.is_empty() {
        return Err(cmd_err!(
            "errors.cli.customEndpointNoModels",
            "the endpoint reports no models, so there is nothing to sell"
        ));
    }

    // Named by the operator, so two endpoints stay tellable apart. Falling back
    // to the host keeps the row stable across re-runs when no name was given.
    let account_id = label
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| host_of(&base));

    let provider = publisher::CUSTOM_PROVIDER;
    let token_ref = keychain::token_ref(provider, &account_id);
    keychain::set(&token_ref, &key).map_err(err)?;
    state.store.set_setting(&publisher::custom_base_key(&account_id), &base).await.map_err(err)?;
    state
        .store
        .set_setting(&publisher::custom_wire_key(&account_id), wire.as_str())
        .await
        .map_err(err)?;
    // The probe's answer is the freshest there is; storing it here means the
    // account is sellable immediately instead of after the first rebuild goes
    // and asks the endpoint again.
    publisher::store_custom_models(&state.store, &account_id, &models).await.map_err(err)?;
    state
        .store
        .upsert_tool(provider, &account_id, &token_ref, &["custom"], "api_key")
        .await
        .map_err(err)?;

    let floor = min_ratio.map(|r| {
        asale_client_core::store::normalise_band(r, asale_client_core::store::RATIO_BAND_FULL.1).0
    });
    let sell_on = enabled.unwrap_or(true);
    // Reuses the ordinary sell path so the terms land exactly where a
    // subscription's do — one place decides what a lane offers, whatever kind
    // of account is behind it.
    set_account_sell(
        state,
        provider.to_string(),
        account_id.clone(),
        sell_on,
        None,
        floor,
        None,
        concurrency,
    )
    .await?;

    // What the market will actually take of that list. The endpoint's own set
    // is usually much larger than the catalog's, and "connected, 400 models,
    // selling 12" is the answer the operator needs to see.
    let sellable = sellable_custom_models(state, &account_id).await;
    Ok(json!({
        "provider": provider,
        "account_id": account_id,
        "base_url": base,
        // Returned whether it was asked for or found, because those are the
        // same answer to the operator and the found one is the news.
        "wire": wire.as_str(),
        "endpoint_models": models.len(),
        "sellable_models": sellable,
        "sell_enabled": sell_on,
    }))
}

/// Every custom endpoint this device has, with what it is selling.
///
/// Separate from `list_accounts` — which lists these too, like any other
/// account — because it answers the question only this kind of account raises:
/// which endpoint is behind it, and how much of what that endpoint serves the
/// platform can actually trade.
pub async fn list_custom_endpoints(state: &AppState) -> R<Value> {
    if !custom_endpoints_enabled() {
        return Err(cmd_err!(
            "errors.cli.customEndpointsDisabled",
            "custom endpoints are turned off on this client (ASALE_CUSTOM_ENDPOINTS=0)"
        ));
    }
    let tools = state.store.list_tools().await.map_err(err)?;
    let mut out = Vec::new();
    for t in tools.iter().filter(|t| t.provider == publisher::CUSTOM_PROVIDER) {
        let base = state
            .store
            .get_setting(&publisher::custom_base_key(&t.account_id))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        out.push(json!({
            "account_id": t.account_id,
            "base_url": base,
            "wire": publisher::custom_wire(&state.store, &t.account_id).await.as_str(),
            "sell_enabled": t.sell_enabled,
            "min_ratio": t.sell_min_ratio,
            "concurrency": t.sell_concurrency,
            "sellable_models": sellable_custom_models(state, &t.account_id).await,
        }));
    }
    Ok(json!({"endpoints": out}))
}

/// The models one custom account is currently advertising — the endpoint's own
/// list, narrowed to what the platform trades. Read back from the pool, so it is
/// the same set the supply declaration is built from rather than a second
/// computation of it that could disagree.
async fn sellable_custom_models(state: &AppState, account_id: &str) -> Vec<String> {
    publisher::rebuild_pool(&state.store, &state.pool).await;
    let Ok(pool) = state.pool.lock() else { return Vec::new() };
    pool.lane_views(now_secs())
        .into_iter()
        .filter(|l| l.provider == publisher::CUSTOM_PROVIDER && l.account_id == account_id)
        .map(|l| l.model)
        .collect()
}

/// The host of a URL, for naming an endpoint connected without a label. Parsed
/// by hand: the point is a short human-facing id, and pulling in a URL crate for
/// one split would be the larger change.
fn host_of(base: &str) -> String {
    let rest = base.split_once("://").map(|(_, r)| r).unwrap_or(base);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if host.is_empty() {
        base.to_string()
    } else {
        host.to_string()
    }
}

/// Connect a metered platform account by pasting its API key — whichever
/// families `providers::PROVIDERS` marks `Credential::ApiKey`.
///
/// This is the pay-as-you-go half of each vendor, not the coding subscription —
/// those use `oauth_device_login`. A platform key never expires and has no
/// refresh token: it *is* the credential. Everything after this point is the
/// same as an OAuth account: the key lives in the encrypted secret store, the
/// local store keeps only a reference, and the sell switch, daily cap and
/// metering all work unchanged.
///
/// The key is verified against the vendor before it is saved. A key that is
/// already dead would otherwise sit in the account list looking connected and
/// fail every task it was matched to, which reads as "asale is broken" and
/// costs reputation on a device that did nothing wrong. A probe that cannot
/// reach the vendor at all is *not* treated as a bad key: offline should not
/// stop someone from connecting an account.
pub async fn connect_api_key(
    state: &AppState,
    provider: String,
    api_key: String,
    label: Option<String>,
) -> R<Value> {
    let p = Provider::from_str_opt(&provider).filter(|p| asale_protocol::ids::is_api_key_provider(*p));
    let Some(p) = p else {
        return Err(cmd_err!(
            "errors.cli.notAnApiKeyProvider",
            "this provider is connected by signing in, not with an API key — \
             pasted keys are for the metered platform APIs, which are the ones \
             `providers::PROVIDERS` marks `Credential::ApiKey`"
        ));
    };
    // The same two shape checks `api_key_cred` makes, restated here so they can
    // carry a translation key: this is a form the user typed into, and "paste
    // just the key, not the whole command" is the one message on this screen
    // that has to land. The library keeps its own guards as an invariant.
    let trimmed = api_key.trim().trim_matches(['"', '\'']).trim();
    if trimmed.is_empty() {
        return Err(cmd_err!("errors.cli.apiKeyEmpty", "API key is empty"));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(cmd_err!(
            "errors.cli.apiKeyHasWhitespace",
            "API key contains whitespace — paste just the key, not the whole command"
        ));
    }
    let cred = cli_import::api_key_cred(&provider, &api_key).map_err(err)?;

    if let Err(e) = verify_api_key(p, &cred.access_token).await {
        return Err(e);
    }

    // A label lets someone tell two keys on the same vendor apart ("work",
    // "personal"); without one the token digest keeps the row stable and
    // distinct, exactly as it does for a CLI import with no resolvable email.
    let account_id = label
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| format!("{provider}-{}", cli_import::token_fingerprint(&cred)));

    keychain::set(&keychain::token_ref(&provider, &account_id), &cred.access_token).map_err(err)?;
    // origin=api_key: not a copy of a locally installed CLI's credential, so
    // the "shared with your CLI" warning does not apply to it.
    state
        .store
        .upsert_tool(&provider, &account_id, &keychain::token_ref(&provider, &account_id), &["api-key"], "api_key")
        .await
        .map_err(err)?;
    accounts_changed(state).await;

    Ok(json!({"provider": provider, "account_id": account_id}))
}

/// Hosts a provider's key may belong to, in probe order.
///
/// Moonshot runs two independent deployments and a key works on exactly one of
/// them, so both are tried: rejecting a perfectly good global key because this
/// device happened to probe the mainland host would be a false alarm the user
/// has no way to argue with. xAI and DeepSeek each have a single host.
///
/// The hosts themselves live in `asale_protocol::providers`, beside the URL the
/// gateway relays to — a key verified against one host and then relayed to
/// another is the failure this pairing exists to make impossible.
fn verify_hosts(p: Provider) -> &'static [&'static str] {
    asale_protocol::spec(p).verify_hosts
}

/// Ask the vendor whether a key is live, using the one endpoint every
/// OpenAI-compatible API exposes for free.
///
/// `Ok(())` also covers "could not tell": only a host that positively refuses
/// the credential is an error, and only when no other host accepted it.
async fn verify_api_key(p: Provider, key: &str) -> R<()> {
    let hosts = verify_hosts(p);
    if hosts.is_empty() {
        return Ok(());
    }
    let mut rejected = false;
    for base in hosts {
        let resp = asale_client_core::http::upstream()
            .get(format!("{base}/models"))
            .header("authorization", format!("Bearer {key}"))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;
        match resp {
            // 401 from this deployment; another may still know the key.
            Ok(r) if r.status().as_u16() == 401 => rejected = true,
            // 403 is deliberately not a rejection: these hosts answer region
            // blocks the same way, and sending the user hunting for a new key
            // when what they need is a proxy wastes the one thing they have.
            // Anything else means the host was reached and did not refuse the
            // credential — a 404 or 429 says nothing about its validity.
            Ok(_) => return Ok(()),
            Err(e) => tracing::warn!(provider = %p, base, "could not verify API key: {e}"),
        }
    }
    if rejected {
        return Err(cmd_err!(
            "errors.cli.apiKeyRejected",
            format!("{p} rejected this API key — check it was copied in full and has not been revoked"),
            provider = p.to_string()
        ));
    }
    // Every host was unreachable. Being offline is not evidence of a bad key.
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The Sell page hides every window-derived figure for these, so the
    /// classifier has to catch all three shapes a key arrives in — including a
    /// key lifted out of `~/.codex/auth.json`, whose only marker is the account
    /// id `cli_import` gives it.
    #[test]
    fn a_key_is_told_apart_from_a_plan_subscription() {
        assert!(is_key_credential("kimi_api", "me@x.com", Some("api_key")));
        assert!(is_key_credential("xai_api", "me@x.com", Some("import")));
        assert!(is_key_credential("codex", "api-key", Some("import")));
        // DeepSeek is key-only, so the provider alone settles it — there is no
        // subscription flavour of it for the origin to disambiguate.
        assert!(is_key_credential("deepseek", "me@x.com", Some("oauth")));
        assert_eq!(verify_hosts(Provider::Deepseek), &["https://api.deepseek.com/v1"]);

        assert!(!is_key_credential("claude", "me@x.com", Some("import")));
        assert!(!is_key_credential("codex", "me@x.com", Some("oauth")));
        assert!(!is_key_credential("kimi", "me@x.com", Some("oauth")));
    }
}
