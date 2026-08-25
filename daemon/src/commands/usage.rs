//! Usage and rate-limit views: what this device sold, bought and used, and
//! how much of each provider's subscription window is left.

use crate::keychain;
use crate::state::AppState;
use asale_client_core::store::{LocalStore, ToolRow};
use asale_client_core::{discovery, Provider};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use super::server_client::authed;
use super::{R, day_start_ts, day_str, err, now_secs};
use crate::cmd_err;

/// What the account *sold* over a period (day / week / month / all): tokens,
/// earnings and call count, for the tray, the dashboard tile and the share card.
///
/// The earnings come from the server's ledger, because that is the only place
/// they exist. A local `provider_records` row is written the moment a call is
/// served — before anyone has priced it — with `amount_usdt = 0`, and nothing
/// fills it in afterwards except a manual "对账" run on the Records page (which
/// only reaches back 150 tasks). So every sell-side earnings figure in the app
/// read 0.00 against a real token count and a real call count, which reads as
/// "you earned nothing" rather than "this number is not known here".
///
/// The reply is cached for [`SOLD_TTL`] and falls back to the local rows when
/// the server cannot be reached, so the tray's few-second poll still costs at
/// most one request per half-minute and an offline device still has tokens and
/// counts to show.
///
/// The subscription category is derived by the UI from `publish_limits` +
/// `list_accounts` (capacity vs. window usage), so it is not repeated here.
pub async fn usage_summary(state: &AppState, period: String) -> R<Value> {
    if let Some(sold) = server_sold_summary(state, &period).await {
        return Ok(json!({ "period": period, "sold": sold, "source": "server" }));
    }
    // Cutoff in unix seconds; `None` = all-time.
    let since: Option<i64> = match period.as_str() {
        "day" => Some(day_start_ts()),
        "week" => Some(now_secs() - 7 * 86400),
        "month" => Some(now_secs() - 30 * 86400),
        _ => None, // "all"
    };
    let sold = state.store.sold_summary(since).await.map_err(err)?;
    Ok(json!({
        "period": period,
        "sold": { "tokens": sold.0, "amount_usdt": sold.1, "count": sold.2 },
        "source": "local",
    }))
}

/// How long a settled-earnings reading counts as current.
const SOLD_TTL: i64 = 30;
/// How long the tray will wait for it. Short, and shorter than the poll it
/// feeds: this is a number in the corner of a status panel, and a server that
/// is taking its time should cost one stale figure rather than a queue of
/// in-flight requests.
const SOLD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// The sell-side headline off the server's ledger, cached per period. `None`
/// means the server did not answer — signed out, offline, or a bad gateway —
/// and the caller should fall back to what this device recorded itself.
async fn server_sold_summary(state: &AppState, period: &str) -> Option<Value> {
    let now = now_secs();
    if let Some((at, cached)) = state.sold_cache.read().await.get(period) {
        if now - at < SOLD_TTL {
            return cached.clone();
        }
    }
    let answer = tokio::time::timeout(
        SOLD_TIMEOUT,
        authed(
            state,
            reqwest::Method::GET,
            &format!("/api/v1/me/usage?role=provider&period={period}"),
            None,
        ),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
    .map(|v| sold_bucket(&v));
    state.sold_cache.write().await.insert(period.to_string(), (now, answer.clone()));
    answer
}

/// The three figures the sell-side headline needs, out of a `/me/usage` body.
///
/// Split out so the field names — the one thing that silently turns a real
/// figure back into 0.00 if the server ever renames them — are covered by a
/// test rather than by a round trip.
fn sold_bucket(v: &Value) -> Value {
    json!({
        "tokens": v["total_tokens"].as_i64().unwrap_or(0),
        // `total_amount` on the provider side is `provider_income` — the money
        // after the platform's cut, which is what was earned.
        "amount_usdt": v["total_amount"].as_i64().unwrap_or(0),
        "count": v["conversations"].as_i64().unwrap_or(0),
    })
}

/// The buy-side usage dashboard, straight from the server's ledger.
///
/// There is no local copy to fall back to, and that is the point: the client's
/// own mirror of the calls it relayed recorded an amount of zero for every one
/// of them (it never learns the price) and took its token counts off the relayed
/// response rather than off what was billed, so on a cached turn it disagreed
/// with the invoice by most of the prompt. A page that cannot be right is worse
/// than a page that says it is offline.
async fn bought_overview(state: &AppState, period: &str) -> R<Value> {
    server_overview(state, "consumer", period).await
}

/// One side of the trade, as the ledger has it.
async fn server_overview(state: &AppState, role: &str, period: &str) -> R<Value> {
    authed(
        state,
        reqwest::Method::GET,
        &format!("/api/v1/me/usage?role={role}&period={period}"),
        None,
    )
    .await
}

/// Full usage dashboard for the Usage page (mirrors TokenTracker's overview):
/// headline totals, per-model breakdown, a per-day table, a heatmap series and
/// rolling stats. `scope` picks the record source — `sold` (provider_records) or
/// `used` (local CLI logs); `bought` is answered by the server, which is the
/// only side that knows what a call cost. `period` bounds the headline / table /
/// model breakdown; the heatmap spans the last ~150 days.
pub async fn usage_overview(state: &AppState, period: String, scope: String) -> R<Value> {
    // The server already returns this page's exact shape, so it is handed
    // through rather than re-derived.
    if scope == "bought" {
        return bought_overview(state, &period).await;
    }
    // The sell side is the ledger's answer too, for the money if nothing else:
    // the local rows carry `amount_usdt = 0` until someone reconciles by hand
    // (see `usage_summary`), so the page's earnings line was either zero or,
    // because it hides a zero, missing. A device that cannot reach the server
    // still has its own tokens and counts, so a failure falls through to the
    // local aggregation rather than emptying the page.
    if scope == "sold" {
        match server_overview(state, "provider", &period).await {
            Ok(v) => return Ok(v),
            Err(e) => tracing::warn!(error = %e.message, "sell-side usage: server unreachable, showing this device's own records"),
        }
    }
    // Fold any newly-inserted ledger rows into the snapshot first (incremental —
    // only rows added since the last run are scanned), then read the snapshot.
    state.store.aggregate_usage().await.map_err(err)?;
    // Fold local AI-CLI session logs so "我使用的" reflects current usage.
    let _ = crate::usage_scan::scan_claude_logs(&state.store).await;

    let sources: &[&str] = match scope.as_str() {
        "sold" => &["sold"],
        _ => &["used"], // "used" (default) = local CLI usage
    };
    let now = now_secs();
    let since: Option<String> = match period.as_str() {
        "day" => Some(day_str(day_start_ts())),
        "week" => Some(day_str(now - 7 * 86400)),
        "month" => Some(day_str(now - 30 * 86400)),
        _ => None, // "total"
    };
    let sd = since.as_deref();

    let (total_tokens, total_amount, conversations) = state.store.agg_totals(sources, sd).await.map_err(err)?;

    let models: Vec<Value> = state
        .store
        .agg_by_model(sources, sd, 12)
        .await
        .map_err(err)?
        .into_iter()
        .map(|(model, tokens, count)| {
            let share = if total_tokens > 0 { tokens as f64 / total_tokens as f64 * 100.0 } else { 0.0 };
            json!({ "model": model, "tokens": tokens, "count": count, "share": share })
        })
        .collect();

    let daily: Vec<Value> = state
        .store
        .agg_by_day(sources, sd)
        .await
        .map_err(err)?
        .into_iter()
        .map(|(date, total, input, output, cache, count)| {
            json!({ "date": date, "total": total, "input": input, "output": output, "cache": cache, "count": count })
        })
        .collect();

    let heatmap_since = day_str(now - 150 * 86400);
    let heatmap: Vec<Value> = state
        .store
        .agg_by_day(sources, Some(&heatmap_since))
        .await
        .map_err(err)?
        .into_iter()
        .map(|(date, total, _i, _o, _c, _n)| json!({ "date": date, "tokens": total }))
        .collect();

    let (active_days, first_day) = state.store.agg_active(sources).await.map_err(err)?;
    let d7 = state.store.agg_totals(sources, Some(&day_str(now - 7 * 86400))).await.map_err(err)?.0;
    let d30 = state.store.agg_totals(sources, Some(&day_str(now - 30 * 86400))).await.map_err(err)?.0;
    let all_time = state.store.agg_totals(sources, None).await.map_err(err)?.0;
    let avg = if active_days > 0 { all_time / active_days } else { 0 };

    Ok(json!({
        "period": period,
        "scope": scope,
        "total_tokens": total_tokens,
        "total_amount": total_amount,
        "conversations": conversations,
        "models": models,
        "daily": daily,
        "heatmap": heatmap,
        "stats": { "d7": d7, "d30": d30, "avg": avg, "active_days": active_days, "first_day": first_day },
    }))
}

/// What is connected per provider: `provider -> (account count, representative
/// plan)`. The plan is a label for the card; nothing is sized against it here.
pub(crate) async fn provider_caps(state: &AppState) -> R<std::collections::HashMap<String, (i64, Option<String>)>> {
    let tools = state.store.list_tools().await.map_err(err)?;
    let mut caps: std::collections::HashMap<String, (i64, Option<String>)> = std::collections::HashMap::new();
    for tool in &tools {
        if Provider::from_str_opt(&tool.provider).is_none() { continue }
        // Prefer the plan captured at OAuth/import time, else the tools column.
        let plan = state
            .store
            .get_setting(&format!("plan:{}:{}", tool.provider, tool.account_id))
            .await
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .or_else(|| tool.plan.clone());
        let entry = caps.entry(tool.provider.clone()).or_insert((0, None));
        entry.0 += 1;
        if entry.1.is_none() {
            entry.1 = plan;
        }
    }
    Ok(caps)
}

/// Rate-limit / quota windows per provider for the Limits page (mirrors
/// TokenTracker's usage-limits payload). Each provider reports whether it is
/// connected, a plan label, and a list of windows with `used_percent`, an
/// absolute `reset_at` (unix seconds), and `window_seconds` so the UI can
/// compute pace markers and reset countdowns. The windows are whichever ones
/// the provider itself keeps — not a fixed 5h/daily pair, which named both of
/// Codex's wrong.
///
/// `live` says whether those windows are the provider's own numbers, `as_of`
/// when they were measured for the providers whose numbers are banked rather
/// than fetched, and `fallback_reason` why they are not the provider's when a
/// read was attempted and failed. Without that last field a geo-blocked
/// upstream (403 "Request not allowed" — the usual state when no upstream proxy
/// is configured) silently degrades to a served-vs-cap estimate that looks like
/// real data and hides whole windows, e.g. the weekly per-model ones.
pub async fn usage_limits(state: &AppState, force: Option<bool>) -> R<Value> {
    let force = force.unwrap_or(false);
    let caps = provider_caps(state).await?;
    let tools = state.store.list_tools().await.map_err(err)?;
    let mut providers = Vec::new();
    for provider in asale_protocol::ids::subscribable_providers().iter().map(|p| p.as_str()) {
        let (accounts, plan) = caps.get(provider).cloned().unwrap_or((0, None));
        if accounts == 0 {
            providers.push(json!({ "id": provider, "connected": false }));
            continue;
        }

        // The provider's *real* rate-limit windows, or none at all.
        //
        // There used to be a fallback here: this device's own sales against a
        // guessed plan cap, drawn in the same bars under an "estimate" chip. It
        // was the same invented number that used to gate the lanes, and it read
        // 100% over subscriptions that were nowhere near spent. A card that says
        // "the vendor publishes nothing here" is the honest version.
        //
        // `stale_reason` and `fallback_reason` are different answers to
        // different questions and must not be collapsed: the first says why a
        // real reading could not be refreshed, the second why there is no real
        // reading at all.
        let (windows, live, as_of, stale_reason, fallback_reason) =
            match live_windows(state, provider, &tools, force).await {
                LiveWindows::Ok { windows, as_of, stale_reason } => (windows, true, as_of, stale_reason, None),
                LiveWindows::Failed(e) => (json!([]), false, None, None, Some(e)),
                // Not an error, but still owed an explanation: without one the
                // empty card is a blank with nothing behind it.
                _ => (json!([]), false, None, None, None),
            };

        providers.push(json!({
            "id": provider,
            "connected": true,
            "plan_label": plan,
            "windows": windows,
            "live": live,
            "as_of": as_of,
            "stale_reason": stale_reason,
            "fallback_reason": fallback_reason,
        }));
    }
    Ok(json!({ "providers": providers }))
}

/// Whether this provider answers a usage endpoint that costs no subscription
/// quota to read — the mechanism that can be asked while the account is idle.
///
/// Each is a different URL answering a different shape, normalised by its own
/// `fetch_*_windows` below; what they have in common is the only thing this
/// predicate is asked about: the reading is free, so it can be taken on a
/// timer rather than bought with a request.
pub(crate) fn has_usage_endpoint(provider: &str) -> bool {
    quota_source(provider) == Some(asale_protocol::QuotaSource::Endpoint)
}

/// Where this provider's real utilisation can be read, per
/// `asale_protocol::providers` — the same field the header-banking path reads,
/// so "can be asked while idle" and "can only be listened to" cannot disagree.
fn quota_source(provider: &str) -> Option<asale_protocol::QuotaSource> {
    Provider::from_str_opt(provider).map(|p| asale_protocol::spec(p).quota)
}

/// The real rate-limit windows for a provider, however that provider can be
/// asked. Three mechanisms exist and they are not interchangeable:
///
///   - Claude, Gemini and Kimi answer a dedicated endpoint that spends no
///     quota (`oauth/usage`, `cloudcode-pa:retrieveUserQuota`,
///     `coding/v1/usages`), so they are fetched live and cached briefly.
///   - Codex has no endpoint a ChatGPT bearer may read; its numbers only ride
///     back on `x-codex-*` headers attached to an accepted API call. Those are
///     banked whenever this device serves a Codex task, and topped up by a
///     minimal probe when the bank is stale.
///   - xAI has neither an endpoint nor a probe worth buying: what it
///     volunteers is an `x-ratelimit-*` block on the responses it already
///     serves, so its readings are banked from the serving path and never
///     refreshed while idle. An account that has not sold anything yet reports
///     nothing, which is the honest answer.
///
/// Everything else reports nothing at all, and says so rather than quietly
/// becoming an estimate.
///
/// Whichever route was taken, a read that fails falls back to the last one that
/// worked before it falls back to the estimate. `oauth/usage` shares Claude
/// Code's own budget and answers 429 for minutes at a time on a busy account —
/// and replacing a real 94% with an estimated 0% for the duration is the worst
/// of the three answers available. Yesterday's real number, labelled with its
/// age, is worth more than a fresh guess.
pub(crate) async fn live_windows(state: &AppState, provider: &str, tools: &[ToolRow], force: bool) -> LiveWindows {
    let outcome = match provider {
        p if has_usage_endpoint(p) => endpoint_windows_cached(state, provider, tools, force).await,
        "codex" => codex_windows(state, provider, tools, force).await,
        // xAI volunteers `x-ratelimit-*` on the responses it serves and answers
        // nothing that could be asked while idle, so the bank is the whole
        // mechanism — there is no probe to fall back on. Codex is header-sourced
        // too but has a probe, which is why it is named above rather than here.
        p if matches!(quota_source(p), Some(asale_protocol::QuotaSource::Headers(_))) => {
            match banked_quota(state, provider, tools).await {
                Some((at, windows)) => LiveWindows::Ok { windows, as_of: Some(at), stale_reason: None },
                None => LiveWindows::Unsupported,
            }
        }
        _ => return LiveWindows::Unsupported,
    };
    if let LiveWindows::Failed(reason) = &outcome {
        if let Some((at, windows)) = banked_quota(state, provider, tools).await {
            return LiveWindows::Ok { windows, as_of: Some(at), stale_reason: Some(reason.clone()) };
        }
    }
    outcome
}

/// Outcome of a live rate-limit read.
pub(crate) enum LiveWindows {
    /// The provider's own windows (a JSON array of LimitWindow). `as_of` is
    /// when they were measured, for readings that are banked rather than
    /// fetched on demand — an hour-old snapshot is still the provider's own
    /// number, but the page has to be able to say how old it is. `stale_reason`
    /// is set when a refresh was attempted and failed, so the age has an
    /// explanation rather than just being a number that stops moving.
    Ok { windows: Value, as_of: Option<i64>, stale_reason: Option<String> },
    /// This provider exposes no usage endpoint we can read.
    Unsupported,
    /// A read was attempted and failed; the string says why, verbatim enough to
    /// act on (HTTP status + upstream message, or the transport error).
    Failed(String),
}

/// Live rate-limit windows from a provider's own usage endpoint, cached per
/// provider (60s on success — Claude's endpoint shares Claude Code's budget and
/// 429s easily; on failure long enough that a page poll every 30s does not
/// queue up 12-second timeouts). `force` bypasses the cache.
///
/// A 429 waits considerably longer than other failures. It is not a transient
/// glitch worth retrying at once — it means this account has spent the budget
/// the endpoint shares with the vendor's own CLI, and it clears in minutes, not
/// seconds. Re-asking every half minute could not succeed and only deepens the
/// hole the account is already in.
pub(crate) async fn endpoint_windows_cached(
    state: &AppState,
    provider: &str,
    tools: &[ToolRow],
    force: bool,
) -> LiveWindows {
    if !has_usage_endpoint(provider) {
        return LiveWindows::Unsupported;
    }
    const TTL: i64 = 60;
    const FAIL_TTL: i64 = 30;
    const RATE_LIMITED_TTL: i64 = 5 * 60;
    let now = now_secs();
    if !force {
        let cache = state.limits_cache.read().await;
        if let Some((at, cached)) = cache.get(provider) {
            let ttl = match cached {
                Ok(_) => TTL,
                Err(e) if is_rate_limited(e) => RATE_LIMITED_TTL,
                Err(_) => FAIL_TTL,
            };
            if now - at < ttl {
                return match cached {
                    Ok(w) => LiveWindows::Ok { windows: w.clone(), as_of: None, stale_reason: None },
                    Err(e) => LiveWindows::Failed(e.clone()),
                };
            }
        }
    }
    // Any account in the family can answer for the subscription, so try each in
    // turn: one CLI-imported row with no token of its own must not blank the
    // page while another account could have answered.
    let mut last_err: Option<String> = None;
    for account in tools.iter().filter(|t| t.provider == provider) {
        let key = keychain::token_ref(provider, &account.account_id);
        let Some(token) = keychain::get(&key).ok().flatten() else {
            last_err = Some(format!("{}: no OAuth token in the local secret store", account.account_id));
            continue;
        };
        match fetch_provider_windows(state, provider, &account.account_id, &token).await {
            Ok(w) => {
                let v = Value::Array(w);
                // Bank it: the next 429 — and on a busy account there will be
                // one — then still has a real reading to show.
                record_quota_windows(&state.store, provider, &account.account_id, &v).await;
                state.limits_cache.write().await.insert(provider.to_string(), (now, Ok(v.clone())));
                return LiveWindows::Ok { windows: v, as_of: None, stale_reason: None };
            }
            Err(e) => last_err = Some(format!("{}: {e}", account.account_id)),
        }
    }
    match last_err {
        Some(reason) => {
            state.limits_cache.write().await.insert(provider.to_string(), (now, Err(reason.clone())));
            LiveWindows::Failed(reason)
        }
        None => LiveWindows::Unsupported, // no accounts in this family at all
    }
}

/// Ask one account's provider for its own rate-limit windows, whichever
/// endpoint that provider keeps them behind.
///
/// The account id is passed because Gemini's read needs somewhere per-account
/// to remember the Code Assist project it resolved, and because the plan a
/// provider volunteers on the way past is worth banking under the account it
/// describes.
pub(crate) async fn fetch_provider_windows(
    state: &AppState,
    provider: &str,
    account_id: &str,
    token: &str,
) -> R<Vec<Value>> {
    match provider {
        p if asale_protocol::ids::is_claude_family(p) => fetch_claude_windows(token).await,
        "gemini" => fetch_gemini_windows(state, account_id, token).await,
        "kimi" => fetch_kimi_windows(token).await,
        _ => Err(cmd_err!("errors.usage.noWindows", format!("{provider} publishes no usage endpoint"))),
    }
}

/// Fetch + normalize Claude's OAuth usage windows into our LimitWindow shape.
/// The `Err` string is user-facing: it is what the Limits page shows to explain
/// an estimate, so it names the status and the upstream's own message.
pub(crate) async fn fetch_claude_windows(access_token: &str) -> R<Vec<Value>> {
    let resp = asale_client_core::http::upstream()
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(12))
        .send()
        .await
        .map_err(|e| {
            cmd_err!(
                "errors.usage.upstreamUnreachable",
                format!("request to api.anthropic.com failed: {e}"),
                detail = e.to_string()
            )
        })?;
    let status = resp.status();
    if !status.is_success() {
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let msg = body.pointer("/error/message").and_then(|v| v.as_str()).unwrap_or_default();
        return Err(match (status.as_u16(), msg) {
            // What a geo-blocked region answers to *every* request, including
            // unauthenticated ones — the fix is an upstream proxy, not a re-login.
            (403, m) if m.contains("not allowed") => cmd_err!(
                "errors.usage.regionBlocked",
                format!("HTTP 403 {m} (region-blocked upstream)"),
                detail = m
            ),
            (_, "") => cmd_err!(
                "errors.usage.upstreamStatus",
                format!("HTTP {status}"),
                status = status.as_u16(),
                detail = ""
            ),
            (code, m) => cmd_err!(
                "errors.usage.upstreamStatus",
                format!("HTTP {code} {m}"),
                status = code,
                detail = m
            ),
        });
    }
    let body: Value = resp.json().await.map_err(|e| cmd_err!("errors.usage.unreadable", format!("unreadable usage response: {e}"), detail = e.to_string()))?;
    let windows = normalize_claude_windows(&body);
    if windows.is_empty() {
        Err(cmd_err!("errors.usage.noWindows", "usage endpoint returned no rate-limit windows"))
    } else {
        Ok(windows)
    }
}

/// Map an `oauth/usage` body onto our LimitWindow shape. Split from the request
/// so the mapping — which windows we surface and how they are labelled — is
/// testable without a token.
pub fn normalize_claude_windows(body: &Value) -> Vec<Value> {
    let mut windows = Vec::new();
    let mk = |key: &str, label: &str, w: &Value, secs: i64| -> Option<Value> {
        let util = w.get("utilization")?.as_f64()?;
        let reset = w.get("resets_at").and_then(|v| v.as_str());
        Some(json!({ "key": key, "label": label, "used_percent": util, "reset_at": reset, "window_seconds": secs }))
    };
    if let Some(w) = body.get("five_hour").filter(|v| v.is_object()) {
        if let Some(x) = mk("5h", "5h", w, 18_000) {
            windows.push(x);
        }
    }
    if let Some(w) = body.get("seven_day").filter(|v| v.is_object()) {
        if let Some(x) = mk("7d", "7d", w, 604_800) {
            windows.push(x);
        }
    }
    if let Some(w) = body.get("seven_day_opus").filter(|v| v.is_object()) {
        if let Some(x) = mk("7d_opus", "7d Opus", w, 604_800) {
            windows.push(x);
        }
    }
    // Model-scoped weekly windows (e.g. "Fable") arrive in the generic `limits`
    // array with kind=weekly_scoped; skip an "Opus" scoped entry that duplicates
    // the legacy seven_day_opus field.
    let has_opus = body.get("seven_day_opus").is_some_and(|v| v.is_object());
    if let Some(arr) = body.get("limits").and_then(|v| v.as_array()) {
        for e in arr {
            if e.get("kind").and_then(|v| v.as_str()) != Some("weekly_scoped") {
                continue;
            }
            let model = e.get("scope").and_then(|s| s.get("model"));
            let label = model
                .and_then(|m| m.get("display_name").and_then(|v| v.as_str()))
                .or_else(|| model.and_then(|m| m.get("id").and_then(|v| v.as_str())));
            let Some(label) = label else { continue };
            if has_opus && label.eq_ignore_ascii_case("opus") {
                continue;
            }
            let Some(util) = e.get("percent").and_then(|v| v.as_f64()) else { continue };
            let reset = e.get("resets_at").and_then(|v| v.as_str());
            windows.push(json!({
                // Prefixed like the fixed windows ("7d Opus"): a bare model name
                // in a column of "5h" / "7d" does not say what it measures.
                "key": format!("ws_{label}"), "label": format!("7d {label}"),
                "used_percent": util, "reset_at": reset, "window_seconds": 604_800,
            }));
        }
    }
    windows
}

// ── Codex ───────────────────────────────────────────────────────────────────
//
// Codex reports its rate-limit state only as `x-codex-*` headers on an accepted
// API call. Everything cheaper was tried and does not work: `codex/usage` and
// `codex/rate_limits` answer a ChatGPT bearer 403 (an HTML edge page, not JSON),
// `codex/models` answers 200 but carries no such headers, and a request the API
// rejects at validation — bad model, empty input — is answered *before* the
// rate-limit middleware runs and so carries none either. So the reading is
// banked whenever this device serves a Codex task and, failing that, bought
// with the smallest request that will be accepted.

/// How long a banked reading counts as current. Long, because refreshing it
/// costs subscription quota and the windows it describes are hours to days
/// wide; the Limits page reports the age either way.
pub(crate) const CODEX_SNAPSHOT_TTL: i64 = 10 * 60;
/// Backoff after a failed probe, so a broken upstream is not re-asked on every
/// 30-second page poll.
const CODEX_PROBE_FAIL_TTL: i64 = 5 * 60;

/// Settings key for the last quota reading taken off a provider's own headers.
pub(crate) fn quota_snapshot_key(provider: &str, account_id: &str) -> String {
    format!("quota_snapshot:{provider}:{account_id}")
}

/// How old a banked reading may be and still gate what this account sells.
///
/// Generous, because the alternative is worse and because the reading does not
/// go stale the way a cache does: [`quota::serviceable_tokens`] keeps
/// subtracting what this device has sold since it was taken, so an ageing
/// snapshot converges on the local estimate instead of drifting away from it.
/// What ages badly is the *other* direction — usage this device cannot see, the
/// operator's own Claude Code session — and an hour bounds that.
pub(crate) const GATE_SNAPSHOT_MAX_AGE: i64 = 3600;

/// The provider's own verdict on what this account may still sell, and when the
/// reading was taken.
///
/// `None` when there is no reading, when it is older than
/// [`GATE_SNAPSHOT_MAX_AGE`], or when every window in it has already reset —
/// the caller falls back to the local plan-cap estimate in all three cases,
/// which is what the client did everywhere before this existed.
///
/// Read per account and never shared across a provider's accounts, unlike
/// [`banked_quota`]: two Claude logins can be two different subscriptions, and
/// borrowing one's reading for the other would take a healthy account off the
/// market because a different one is spent.
pub(crate) async fn account_quota_gate(
    store: &LocalStore,
    provider: &str,
    account_id: &str,
    now: i64,
) -> Option<(asale_client_core::quota::QuotaGate, i64)> {
    let raw = store.get_setting(&quota_snapshot_key(provider, account_id)).await.ok().flatten()?;
    let snap: Value = serde_json::from_str(&raw).ok()?;
    let at = snap.get("at").and_then(Value::as_i64)?;
    if now - at > GATE_SNAPSHOT_MAX_AGE {
        return None;
    }
    let windows = snap.get("windows")?.as_array()?;
    let gate = asale_client_core::quota::gate_from_windows(windows, now)?;
    Some((gate, at))
}

/// Bank the windows carried by a provider's response headers.
///
/// Called from the serving path (`RecordSink::observe_quota`) and from the
/// probe, so both routes leave the same record behind.
pub(crate) async fn record_quota_headers(
    store: &LocalStore,
    provider: &str,
    account_id: &str,
    headers: &BTreeMap<String, String>,
) {
    // Which headers carry a reading is the table's answer; how to read them is
    // not — Codex publishes its own `x-codex-*` shape and the conventional
    // `x-ratelimit-*` block needs its per-minute burst limits thrown away, so
    // each keeps its own parser.
    let windows = match quota_source(provider) {
        Some(asale_protocol::QuotaSource::Headers("x-codex-")) => normalize_codex_headers(headers, now_secs()),
        Some(asale_protocol::QuotaSource::Headers(_)) => normalize_ratelimit_headers(headers, now_secs()),
        _ => return,
    };
    record_quota_windows(store, provider, account_id, &Value::Array(windows)).await;
}

/// Bank a reading, however it was obtained — Codex's response headers or
/// Claude's usage endpoint. What makes it worth storing is the same either way:
/// it is the provider's own number, and the next time the provider cannot be
/// reached it is the best answer left.
pub(crate) async fn record_quota_windows(store: &LocalStore, provider: &str, account_id: &str, windows: &Value) {
    if !windows.as_array().is_some_and(|a| !a.is_empty()) {
        return;
    }
    let snap = json!({ "at": now_secs(), "windows": windows });
    let _ = store.set_setting(&quota_snapshot_key(provider, account_id), &snap.to_string()).await;
}

/// Whether a failure reason is an upstream rate limit. These strings are built
/// by `fetch_claude_windows` a few screens down, so this reads our own output
/// rather than guessing at the upstream's wording.
fn is_rate_limited(reason: &str) -> bool {
    reason.contains("HTTP 429")
}

/// The freshest banked reading across a provider's accounts, as `(measured_at,
/// windows)`. Accounts on one subscription report the same numbers, so the
/// newest answer is the right one rather than something to merge.
async fn banked_quota(state: &AppState, provider: &str, tools: &[ToolRow]) -> Option<(i64, Value)> {
    let mut best: Option<(i64, Value)> = None;
    for tool in tools.iter().filter(|t| t.provider == provider) {
        // `continue`, not `?`: one account with nothing banked must not hide a
        // reading another account already has.
        let Some(raw) = state.store.get_setting(&quota_snapshot_key(provider, &tool.account_id)).await.ok().flatten()
        else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else { continue };
        let at = v.get("at").and_then(Value::as_i64).unwrap_or(0);
        let Some(windows) = v.get("windows").filter(|w| w.as_array().is_some_and(|a| !a.is_empty())) else { continue };
        if best.as_ref().is_none_or(|(prev, _)| at > *prev) {
            best = Some((at, windows.clone()));
        }
    }
    best
}

/// Codex windows: the banked reading if it is current, else a probe.
async fn codex_windows(state: &AppState, provider: &str, tools: &[ToolRow], force: bool) -> LiveWindows {
    let now = now_secs();
    if !force {
        if let Some((at, windows)) = banked_quota(state, provider, tools).await {
            if now - at < CODEX_SNAPSHOT_TTL {
                return LiveWindows::Ok { windows, as_of: Some(at), stale_reason: None };
            }
        }
        // A probe that just failed is not retried on the next page poll.
        // `live_windows` substitutes the stale-but-real reading, if there is
        // one, for whatever this returns.
        let cache = state.limits_cache.read().await;
        if let Some((at, Err(e))) = cache.get(provider) {
            if now - at < CODEX_PROBE_FAIL_TTL {
                return LiveWindows::Failed(e.clone());
            }
        }
    }

    match probe_codex_windows(state, provider, tools).await {
        Ok(windows) => {
            state.limits_cache.write().await.remove(provider);
            LiveWindows::Ok { windows, as_of: Some(now_secs()), stale_reason: None }
        }
        Err(reason) => {
            state.limits_cache.write().await.insert(provider.to_string(), (now, Err(reason.clone())));
            LiveWindows::Failed(reason)
        }
    }
}

/// Buy one reading of Codex's rate-limit state with the smallest request the
/// backend will accept, and bank it.
///
/// The response is dropped the moment its headers land, so the stream is torn
/// down before the model has produced anything of consequence — the cost is the
/// handful of input tokens, against a window measured in millions.
async fn probe_codex_windows(state: &AppState, provider: &str, tools: &[ToolRow]) -> Result<Value, String> {
    let mut last_err: Option<String> = None;
    for tool in tools.iter().filter(|t| t.provider == provider) {
        match probe_codex_account(state, provider, &tool.account_id).await {
            Ok(windows) => return Ok(windows),
            Err(e) => last_err = Some(format!("{}: {e}", tool.account_id)),
        }
    }
    Err(last_err.unwrap_or_else(|| "no codex account to ask".into()))
}

/// The same purchase, for one named account, banked under that account's key.
///
/// Split out from the loop above because the two callers want different things
/// from a family of Codex logins: the Limits page shows one row per provider and
/// stops at the first account that answers, while the sell-side gate needs each
/// selling account's own reading — two ChatGPT logins can be two different
/// subscriptions, and one's headroom says nothing about the other's.
pub(crate) async fn probe_codex_account(state: &AppState, provider: &str, account_id: &str) -> Result<Value, String> {
    let token = keychain::get(&keychain::token_ref(provider, account_id))
        .ok()
        .flatten()
        .ok_or_else(|| "no OAuth token in the local secret store".to_string())?;
    let chatgpt_id = asale_client_core::executor::chatgpt_account_id(&token).unwrap_or_default();
    let model = codex_probe_model(state, &token, &chatgpt_id).await?;
    let headers = match fetch_codex_headers(&token, &chatgpt_id, &model).await {
        Ok(h) => h,
        Err(e) => {
            // The entitled model set moves with each Codex release, so a
            // rejected slug means the cached pick has aged out, not that the
            // account is broken. Drop it and let the next attempt re-ask.
            if e.contains("not supported when using Codex") {
                let _ = state.store.set_setting(CODEX_PROBE_MODEL_KEY, "").await;
            }
            return Err(e);
        }
    };
    let windows = normalize_codex_headers(&headers, now_secs());
    if windows.is_empty() {
        return Err("response carried no x-codex-* rate-limit headers".into());
    }
    record_quota_headers(&state.store, provider, account_id, &headers).await;
    Ok(Value::Array(windows))
}

/// When this account's banked reading was taken, if it has one at all.
///
/// Cheaper than [`account_quota_gate`] and answers a different question: not
/// "what may this account still sell" but "is it time to buy a fresh reading".
pub(crate) async fn quota_snapshot_at(store: &LocalStore, provider: &str, account_id: &str) -> Option<i64> {
    let raw = store.get_setting(&quota_snapshot_key(provider, account_id)).await.ok().flatten()?;
    serde_json::from_str::<Value>(&raw).ok()?.get("at").and_then(Value::as_i64)
}

/// Cached slug for the probe request.
const CODEX_PROBE_MODEL_KEY: &str = "codex:probe_model";

/// A model slug this account is entitled to. The set is per-account and per
/// release, so it is asked for rather than assumed — `codex/models` is free and
/// the answer is cached until a request is refused for naming it.
async fn codex_probe_model(state: &AppState, token: &str, account_id: &str) -> Result<String, String> {
    if let Some(m) = state.store.get_setting(CODEX_PROBE_MODEL_KEY).await.ok().flatten().filter(|s| !s.is_empty()) {
        return Ok(m);
    }
    let models = discovery::codex_servable_models(token, account_id).await.map_err(|e| e.to_string())?;
    let model = models.into_iter().next().ok_or("this account is not entitled to the Codex surface")?;
    let _ = state.store.set_setting(CODEX_PROBE_MODEL_KEY, &model).await;
    Ok(model)
}

/// One accepted Codex call, kept only for its headers.
async fn fetch_codex_headers(
    token: &str,
    account_id: &str,
    model: &str,
) -> Result<BTreeMap<String, String>, String> {
    let body = json!({
        "model": model,
        "input": [{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "." }] }],
        "stream": true,
        "store": false,
        "instructions": "Reply with a single period.",
        "reasoning": { "effort": "low" },
    });
    let mut req = asale_client_core::http::upstream()
        .post("https://chatgpt.com/backend-api/codex/responses")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("openai-beta", "responses=experimental")
        .header("originator", "codex_cli_rs")
        .header("user-agent", format!("codex_cli_rs/{}", discovery::CODEX_CLIENT_VERSION))
        .header("session_id", uuid::Uuid::new_v4().to_string())
        .header("authorization", format!("Bearer {token}"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(20));
    if !account_id.is_empty() {
        req = req.header("chatgpt-account-id", account_id);
    }
    let resp = req.send().await.map_err(|e| format!("request to chatgpt.com failed: {e}"))?;
    let status = resp.status();
    let headers = asale_client_core::executor::quota_headers("codex", resp.headers());
    if !status.is_success() {
        // A 429 *is* a reading. The refusal carries the same `x-codex-*`
        // headers an accepted call would, and "spent, back at 14:20" is exactly
        // what was being asked for — treating it as a failed probe would leave
        // the one account that most needs a fresh number stuck on the local
        // estimate, which thinks the window is free. Any other status is just a
        // failure.
        if status.as_u16() == 429 && headers.contains_key("x-codex-primary-used-percent") {
            return Ok(headers);
        }
        let body = resp.text().await.unwrap_or_default();
        let detail: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let msg = detail
            .pointer("/detail")
            .or_else(|| detail.pointer("/error/message"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| body.trim())
            .chars()
            .take(200)
            .collect::<String>();
        return Err(format!("HTTP {status} {msg}"));
    }
    // Headers in hand; dropping the response aborts the stream before the model
    // gets far enough to cost anything worth counting.
    drop(resp);
    Ok(headers)
}

/// Map Codex's `x-codex-*` headers onto our LimitWindow shape.
///
/// Codex names its windows by ordinal — "primary" and "secondary" — not by
/// duration, and which is which moves with the plan and the active limit: a
/// Plus account on the premium limit reports its 7-day window as the primary
/// and leaves the secondary unset. So the label has to be derived from
/// `window-minutes`, and a slot reporting zero minutes is one the account does
/// not have rather than a window sitting at 0%.
pub fn normalize_codex_headers(h: &BTreeMap<String, String>, now: i64) -> Vec<Value> {
    let num = |k: String| -> Option<f64> { h.get(&k)?.parse::<f64>().ok() };
    let mut out = Vec::new();
    for slot in ["primary", "secondary"] {
        let minutes = num(format!("x-codex-{slot}-window-minutes")).unwrap_or(0.0);
        if minutes <= 0.0 {
            continue;
        }
        let Some(pct) = num(format!("x-codex-{slot}-used-percent")) else { continue };
        let seconds = (minutes * 60.0) as i64;
        // The absolute reset first: `reset-after-seconds` is only meaningful
        // beside the instant it was read, which a banked snapshot no longer is.
        let reset_at = num(format!("x-codex-{slot}-reset-at"))
            .map(|v| v as i64)
            .filter(|v| *v > 0)
            .or_else(|| num(format!("x-codex-{slot}-reset-after-seconds")).map(|v| now + v as i64));
        let label = window_label(seconds);
        out.push(json!({
            "key": label, "label": label,
            "used_percent": pct.clamp(0.0, 100.0),
            "reset_at": reset_at, "window_seconds": seconds,
        }));
    }
    out
}

/// A window's duration is the only name Codex gives it. Render it the way the
/// fixed Claude windows are labelled ("5h", "7d"), so one column reads the same
/// whichever provider filled it.
fn window_label(seconds: i64) -> String {
    if seconds % 86_400 == 0 {
        format!("{}d", seconds / 86_400)
    } else if seconds % 3_600 == 0 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}m", (seconds / 60).max(1))
    }
}

// ── Gemini (Code Assist) ────────────────────────────────────────────────────
//
// The Gemini login this client performs is gemini-cli's own installed-app
// OAuth, which means the subscription behind it is Code Assist — and Code
// Assist answers `:retrieveUserQuota` with the very buckets gemini-cli's quota
// display reads. It spends no model quota, so it is polled on a timer like
// Claude's rather than bought like Codex's.
//
// The call is addressed to a project, which `:loadCodeAssist` is what answers
// with: a Google-managed one on the free tier, the operator's own on a paid
// one. That value is stable for months, so it is resolved once per account and
// remembered — re-asking every poll would double the request count for a string
// that only changes when somebody moves projects.

/// Code Assist's private API, the surface gemini-cli itself calls.
const CODE_ASSIST: &str = "https://cloudcode-pa.googleapis.com/v1internal";

/// Settings key holding the resolved Code Assist project for an account.
fn gemini_project_key(account_id: &str) -> String {
    format!("gemini_project:{account_id}")
}

/// One Code Assist call. The methods are `:`-suffixed RPC names, not paths.
async fn code_assist_post(token: &str, method: &str, body: Value) -> R<Value> {
    let resp = asale_client_core::http::upstream()
        .post(format!("{CODE_ASSIST}:{method}"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("user-agent", "gemini-cli/1.0")
        .json(&body)
        .timeout(std::time::Duration::from_secs(12))
        .send()
        .await
        .map_err(|e| {
            cmd_err!(
                "errors.usage.upstreamUnreachable",
                format!("request to cloudcode-pa.googleapis.com failed: {e}"),
                detail = e.to_string()
            )
        })?;
    let status = resp.status();
    if !status.is_success() {
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let msg = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect::<String>();
        return Err(cmd_err!(
            "errors.usage.upstreamStatus",
            format!("HTTP {status} {msg}"),
            status = status.as_u16(),
            detail = msg
        ));
    }
    resp.json().await.map_err(|e| {
        cmd_err!("errors.usage.unreadable", format!("unreadable {method} response: {e}"), detail = e.to_string())
    })
}

/// The Code Assist project this account is onboarded onto, resolved once.
///
/// Banks the tier on the way past. Gemini's OAuth exchange carries no plan
/// either — the same gap that sized every Claude login as the lowest paid tier
/// — and `loadCodeAssist` is the one call that names it, so reading it here
/// costs nothing and re-sizes the account's declared capacity.
async fn gemini_project(state: &AppState, account_id: &str, token: &str) -> R<String> {
    let key = gemini_project_key(account_id);
    if let Some(p) = state.store.get_setting(&key).await.ok().flatten().filter(|s| !s.is_empty()) {
        return Ok(p);
    }
    let body = code_assist_post(
        token,
        "loadCodeAssist",
        json!({ "metadata": { "ideType": "IDE_UNSPECIFIED", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI" } }),
    )
    .await?;
    // The paid tier's name is the one that says "Pro" or "Ultra"; the current
    // tier's id (`free-tier`, `standard-tier`) is the fallback that at least
    // separates a free login from a paid one.
    let plan = body
        .pointer("/paidTier/name")
        .or_else(|| body.pointer("/currentTier/name"))
        .or_else(|| body.pointer("/currentTier/id"))
        .and_then(Value::as_str);
    if let Some(plan) = plan.filter(|s| !s.is_empty()) {
        let _ = state.store.set_setting(&format!("plan:gemini:{account_id}"), plan).await;
    }
    let project = body
        .get("cloudaicompanionProject")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            cmd_err!("errors.usage.noWindows", "loadCodeAssist named no Code Assist project for this account")
        })?
        .to_string();
    let _ = state.store.set_setting(&key, &project).await;
    Ok(project)
}

/// Fetch + normalize Gemini's Code Assist quota buckets.
///
/// A project that has gone stale (the account moved, the managed project was
/// re-issued) surfaces as a failure on the second call rather than the first,
/// so the cached value is dropped on any failure there and the next poll
/// re-resolves it. Cheaper than validating it every time, and self-healing.
pub(crate) async fn fetch_gemini_windows(state: &AppState, account_id: &str, token: &str) -> R<Vec<Value>> {
    let project = gemini_project(state, account_id, token).await?;
    let body = match code_assist_post(token, "retrieveUserQuota", json!({ "project": project })).await {
        Ok(b) => b,
        Err(e) => {
            let _ = state.store.set_setting(&gemini_project_key(account_id), "").await;
            return Err(e);
        }
    };
    let windows = normalize_gemini_buckets(&body);
    if windows.is_empty() {
        Err(cmd_err!("errors.usage.noWindows", "retrieveUserQuota returned no quota buckets"))
    } else {
        Ok(windows)
    }
}

/// Map `retrieveUserQuota`'s buckets onto our LimitWindow shape.
///
/// Code Assist reports headroom as a *remaining* fraction, and per model: one
/// bucket for Pro, one for Flash, and so on. That maps onto the scoped windows
/// Anthropic uses for Opus — a spent Pro bucket must take Pro off the market
/// and leave Flash selling — so a bucket naming a model becomes `ws_<model>`.
///
/// A synthetic account-wide window is added when every bucket is model-scoped,
/// which is the usual answer. Without one the gate has nothing that applies to
/// the whole subscription and falls back to the local estimate, which is the
/// reading this whole path exists to replace. It takes the *most generous*
/// bucket: an account whose Pro quota is gone still has Flash to sell, and only
/// when the last bucket empties is the subscription really spent.
pub fn normalize_gemini_buckets(body: &Value) -> Vec<Value> {
    let Some(buckets) = body.get("buckets").and_then(Value::as_array) else { return Vec::new() };
    let mut out = Vec::new();
    let mut account_wide = false;
    let mut freest: Option<(f64, Option<Value>)> = None;
    for b in buckets {
        // `remainingFraction` is the field gemini-cli's own display reads; the
        // amount beside it is a count whose denominator is not published, so it
        // cannot be turned into a percentage on its own.
        let Some(free) = b.get("remainingFraction").and_then(Value::as_f64) else { continue };
        let used = ((1.0 - free) * 100.0).clamp(0.0, 100.0);
        let reset = b.get("resetTime").cloned().filter(|v| !v.is_null());
        // Code Assist's individual quotas roll over daily; the reset instant is
        // published but its window length is not, and the UI needs one to draw
        // a pace marker against.
        let seconds = 86_400;
        match b.get("modelId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            Some(model) => {
                if freest.as_ref().is_none_or(|(prev, _)| free > *prev) {
                    freest = Some((free, reset.clone()));
                }
                out.push(json!({
                    "key": format!("ws_{model}"), "label": format!("1d {model}"),
                    "used_percent": used, "reset_at": reset, "window_seconds": seconds,
                }));
            }
            None => {
                account_wide = true;
                out.push(json!({
                    "key": window_label(seconds), "label": window_label(seconds),
                    "used_percent": used, "reset_at": reset, "window_seconds": seconds,
                }));
            }
        }
    }
    if !account_wide {
        if let Some((free, reset)) = freest {
            out.push(json!({
                "key": window_label(86_400), "label": window_label(86_400),
                "used_percent": ((1.0 - free) * 100.0).clamp(0.0, 100.0),
                "reset_at": reset, "window_seconds": 86_400,
            }));
        }
    }
    out
}

// ── Kimi Code ───────────────────────────────────────────────────────────────
//
// Kimi Code publishes `coding/v1/usages` — the numbers `kimi-cli`'s own
// `/usage` command prints — and reading it costs no quota, so it is polled.
//
// Its windows are counted in *requests* rather than tokens, which the gate
// handles because it works in fractions of a window and never in absolute
// headroom: "62% of the 5h window is gone" decides exactly as well whether this
// account should keep selling, whatever the unit behind it.

/// Fetch + normalize Kimi Code's usage windows.
pub(crate) async fn fetch_kimi_windows(token: &str) -> R<Vec<Value>> {
    // `/usages` is the current spelling; older deployments answer `/usage` and
    // 404 the plural. Only a 404 falls through to the singular — a 401 means
    // the credential is wrong at both spellings, and asking twice would double
    // the traffic of every poll on an account that is simply logged out.
    let body = match kimi_usage_body(token, "https://api.kimi.com/coding/v1/usages").await {
        Ok(body) => body,
        Err(e) if e.message.contains("HTTP 404") => {
            kimi_usage_body(token, "https://api.kimi.com/coding/v1/usage").await?
        }
        Err(e) => return Err(e),
    };
    let windows = normalize_kimi_usage(&body);
    if windows.is_empty() {
        Err(cmd_err!("errors.usage.noWindows", "the usage endpoint returned no rate-limit windows"))
    } else {
        Ok(windows)
    }
}

async fn kimi_usage_body(token: &str, url: &str) -> R<Value> {
    let resp = asale_client_core::http::upstream()
        .get(url)
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/json")
        .header("user-agent", "kimi-cli/1.0")
        .timeout(std::time::Duration::from_secs(12))
        .send()
        .await
        .map_err(|e| {
            cmd_err!(
                "errors.usage.upstreamUnreachable",
                format!("request to api.kimi.com failed: {e}"),
                detail = e.to_string()
            )
        })?;
    let status = resp.status();
    if !status.is_success() {
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let msg = body
            .pointer("/error/message")
            .or_else(|| body.pointer("/message"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect::<String>();
        return Err(cmd_err!(
            "errors.usage.upstreamStatus",
            format!("HTTP {status} {msg}"),
            status = status.as_u16(),
            detail = msg
        ));
    }
    resp.json().await.map_err(|e| {
        cmd_err!("errors.usage.unreadable", format!("unreadable usage response: {e}"), detail = e.to_string())
    })
}

/// Map a Kimi Code usage body onto our LimitWindow shape.
///
/// Two shapes are in the wild and the endpoint answers whichever its deployment
/// speaks, so both are read:
///
///   * `{"data":[{"model_name":"all","used":..,"limit":..,"resetTime":..}]}` —
///     one row per scope, `all` being the subscription-wide one.
///   * `{"usage":{..},"limits":[{"window":{"duration":5,"timeUnit":"HOUR"},
///     "detail":{"used":..,"limit":..}}]}` — a summary plus one row per window.
///
/// Everything either shape can be missing is: a row with no limit to divide by
/// is dropped rather than reported as 0%, which would read as a free window.
pub fn normalize_kimi_usage(body: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let mut push = |key: String, label: String, used: f64, limit: f64, reset: Option<Value>, secs: i64| {
        if limit <= 0.0 {
            return;
        }
        out.push(json!({
            "key": key, "label": label,
            "used_percent": (used / limit * 100.0).clamp(0.0, 100.0),
            "reset_at": reset, "window_seconds": secs,
        }));
    };
    // `used` is not always sent; `limit - remaining` is the same number.
    let used_of = |v: &Value| -> Option<f64> {
        let limit = num_field(v, &["limit", "limit_amount", "total", "totalQueries"]);
        let used = num_field(v, &["used", "used_amount", "usage"]).or_else(|| {
            let remaining = num_field(v, &["remaining", "remainingQueries", "remaining_amount"])?;
            Some(limit? - remaining)
        })?;
        Some(used)
    };

    if let Some(rows) = body.get("data").and_then(Value::as_array) {
        for row in rows {
            let Some(limit) = num_field(row, &["limit", "limit_amount", "total", "totalQueries"]) else { continue };
            let Some(used) = used_of(row) else { continue };
            let secs = kimi_window_seconds(row).unwrap_or(604_800);
            let reset = kimi_reset(row);
            match row.get("model_name").and_then(Value::as_str) {
                // The subscription-wide row. Everything else is one model's own
                // allowance, which is a scope block rather than a gate.
                Some("all") | None => push(window_label(secs), window_label(secs), used, limit, reset, secs),
                Some(model) => {
                    push(format!("ws_{model}"), format!("{} {model}", window_label(secs)), used, limit, reset, secs)
                }
            }
        }
    }
    if let Some(rows) = body.get("limits").and_then(Value::as_array) {
        for row in rows {
            let detail = row.get("detail").filter(|v| v.is_object()).unwrap_or(row);
            let Some(limit) = num_field(detail, &["limit", "limit_amount", "total", "totalQueries"]) else { continue };
            let Some(used) = used_of(detail) else { continue };
            let secs = kimi_window_seconds(row.get("window").unwrap_or(row)).unwrap_or(18_000);
            push(window_label(secs), window_label(secs), used, limit, kimi_reset(detail).or_else(|| kimi_reset(row)), secs);
        }
    }
    out
}

/// First of `names` that holds a number, however the vendor spelled it — some
/// deployments send these as strings.
fn num_field(v: &Value, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|n| match v.get(*n) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    })
}

/// A Kimi window's length, from the `duration` + `timeUnit` pair its limit rows
/// carry.
fn kimi_window_seconds(v: &Value) -> Option<i64> {
    let duration = num_field(v, &["duration", "window_duration"])? as i64;
    if duration <= 0 {
        return None;
    }
    let unit = v
        .get("timeUnit")
        .or_else(|| v.get("time_unit"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    let per = if unit.contains("MINUTE") {
        60
    } else if unit.contains("HOUR") {
        3_600
    } else if unit.contains("DAY") {
        86_400
    } else if unit.contains("WEEK") {
        604_800
    } else if unit.contains("MONTH") {
        30 * 86_400
    } else if unit.contains("SECOND") {
        1
    } else {
        return None;
    };
    Some(duration * per)
}

/// A Kimi reset instant, as an absolute value our window shape can carry —
/// either the timestamp the row states or the offset it counts down.
fn kimi_reset(v: &Value) -> Option<Value> {
    if let Some(t) = v.get("resetTime").or_else(|| v.get("reset_at")).or_else(|| v.get("reset_time")) {
        if !t.is_null() {
            return Some(t.clone());
        }
    }
    let secs = num_field(v, &["reset_in", "resetIn", "reset_after_seconds"])?;
    Some(json!(now_secs() + secs as i64))
}

// ── OpenAI-style rate-limit headers (xAI) ───────────────────────────────────

/// Below this, a window is a burst limit rather than a subscription window.
///
/// The `x-ratelimit-*` block covers both: the same three header names describe
/// "60 requests a minute" and "your weekly allowance", and only the reset
/// horizon separates them. Reading a per-minute limit as a subscription window
/// would take an account off the market for being *busy*, which is the opposite
/// of what the gate is for — so short windows are reported to nobody.
const BURST_WINDOW_SECS: i64 = 15 * 60;

/// Map an OpenAI-style `x-ratelimit-*` block onto our LimitWindow shape.
///
/// Both dimensions are kept where both are sent: requests and tokens run out
/// independently, and the gate takes whichever is tighter.
pub fn normalize_ratelimit_headers(h: &BTreeMap<String, String>, now: i64) -> Vec<Value> {
    let mut out = Vec::new();
    for dim in ["requests", "tokens"] {
        let num = |k: String| -> Option<f64> { h.get(&k)?.parse::<f64>().ok() };
        let Some(limit) = num(format!("x-ratelimit-limit-{dim}")) else { continue };
        let Some(remaining) = num(format!("x-ratelimit-remaining-{dim}")) else { continue };
        if limit <= 0.0 {
            continue;
        }
        let Some(reset_at) = h.get(&format!("x-ratelimit-reset-{dim}")).and_then(|s| parse_reset_field(s, now)) else {
            continue;
        };
        let horizon = reset_at - now;
        if horizon < BURST_WINDOW_SECS {
            continue;
        }
        let seconds = window_from_horizon(horizon);
        out.push(json!({
            // Keyed by duration like every other provider's account-wide window
            // — the two dimensions usually share a reset, so they usually share
            // a key, and the gate takes whichever of them is tighter. The label
            // is what tells them apart on the page.
            "key": window_label(seconds), "label": format!("{} {dim}", window_label(seconds)),
            "used_percent": ((limit - remaining) / limit * 100.0).clamp(0.0, 100.0),
            "reset_at": reset_at, "window_seconds": seconds,
        }));
    }
    out
}

/// The window a reset horizon most likely belongs to.
///
/// The header block says when the allowance comes back, never how wide it is —
/// a weekly pool read on a Wednesday reports three days. Naming the window
/// after that would relabel the same window every hour, so the horizon is
/// rounded up onto the ladder of durations vendors actually use. It is an
/// inference and only the *label* and the pace marker rest on it; the
/// utilisation and the reset instant either side of it are the vendor's own.
fn window_from_horizon(horizon: i64) -> i64 {
    const LADDER: [i64; 5] = [3_600, 18_000, 86_400, 604_800, 30 * 86_400];
    LADDER.into_iter().find(|w| *w >= horizon).unwrap_or(30 * 86_400)
}

/// An `x-ratelimit-reset-*` value as an absolute instant.
///
/// Three spellings are in circulation and they are not distinguishable by
/// anything but magnitude and suffix: a unix timestamp, a bare number of
/// seconds from now, and OpenAI's duration string (`6m0s`, `1h30m`, `250ms`).
fn parse_reset_field(raw: &str, now: i64) -> Option<i64> {
    let s = raw.trim();
    if let Ok(n) = s.parse::<f64>() {
        let n = n as i64;
        // A plain number is a unix instant if it lands anywhere near now, and a
        // relative offset otherwise. Nothing sends a window a decade wide.
        return Some(if n > now / 2 { n } else { now + n });
    }
    let mut total = 0f64;
    let mut num = String::new();
    let mut unit = String::new();
    let flush = |num: &mut String, unit: &mut String, total: &mut f64| -> bool {
        if num.is_empty() {
            return unit.is_empty();
        }
        let Ok(v) = num.parse::<f64>() else { return false };
        let per = match unit.as_str() {
            "ms" => 0.001,
            "s" | "" => 1.0,
            "m" => 60.0,
            "h" => 3_600.0,
            "d" => 86_400.0,
            _ => return false,
        };
        *total += v * per;
        num.clear();
        unit.clear();
        true
    };
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            if !unit.is_empty() && !flush(&mut num, &mut unit, &mut total) {
                return None;
            }
            num.push(c);
        } else if c.is_ascii_alphabetic() {
            unit.push(c.to_ascii_lowercase());
        } else {
            return None;
        }
    }
    if !flush(&mut num, &mut unit, &mut total) || total <= 0.0 {
        return None;
    }
    Some(now + total.round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `/me/usage?role=provider` body, as the server writes it. The
    /// earnings live under `total_amount` — reading any other name gives back
    /// the 0.00 this whole path exists to stop showing.
    #[test]
    fn the_sell_headline_reads_the_ledgers_own_field_names() {
        let body = json!({
            "period": "all", "scope": "sold",
            "total_tokens": 18_062_379_i64,
            "total_amount": 32_991_046_i64,
            "conversations": 2562,
            "models": [], "daily": [], "heatmap": [],
            "stats": { "d7": 0, "d30": 0, "avg": 0, "active_days": 1, "first_day": "2026-08-01" },
        });
        assert_eq!(
            sold_bucket(&body),
            json!({ "tokens": 18_062_379_i64, "amount_usdt": 32_991_046_i64, "count": 2562 })
        );
    }

    /// An account that has never sold anything answers with zeros, not with
    /// missing fields — and a body that is missing them anyway must read as
    /// zero rather than panic.
    #[test]
    fn a_body_without_the_fields_reads_as_zero() {
        assert_eq!(
            sold_bucket(&json!({})),
            json!({ "tokens": 0, "amount_usdt": 0, "count": 0 })
        );
    }

    /// Shape of a real `oauth/usage` body: two fixed windows plus a model-scoped
    /// weekly one in the generic `limits` array.
    #[test]
    fn normalizes_fixed_and_model_scoped_windows() {
        let body = json!({
            "five_hour": { "utilization": 0.0, "resets_at": "2026-07-26T15:00:00Z" },
            "seven_day": { "utilization": 49.0, "resets_at": "2026-07-30T10:00:00Z" },
            "limits": [
                { "kind": "weekly", "percent": 12.0 },
                {
                    "kind": "weekly_scoped", "percent": 29.0, "resets_at": "2026-07-30T10:00:00Z",
                    "scope": { "model": { "id": "fable", "display_name": "Fable" } }
                }
            ]
        });
        let w = normalize_claude_windows(&body);
        let labels: Vec<&str> = w.iter().map(|x| x["label"].as_str().unwrap()).collect();
        assert_eq!(labels, ["5h", "7d", "7d Fable"]);
        assert_eq!(w[2]["key"], "ws_Fable");
        assert_eq!(w[2]["used_percent"], 29.0);
        assert_eq!(w[2]["window_seconds"], 604_800);
    }

    /// The legacy `seven_day_opus` field and a scoped "Opus" entry are the same
    /// window; surfacing both would double-count it in the list.
    #[test]
    fn drops_scoped_opus_duplicate() {
        let body = json!({
            "seven_day_opus": { "utilization": 3.0, "resets_at": "2026-07-30T10:00:00Z" },
            "limits": [{
                "kind": "weekly_scoped", "percent": 3.0,
                "scope": { "model": { "display_name": "Opus" } }
            }]
        });
        let labels: Vec<String> = normalize_claude_windows(&body)
            .iter()
            .map(|x| x["label"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(labels, ["7d Opus"]);
    }

    /// An error body must not read as "0% used": no windows means the caller
    /// falls back and labels the result an estimate.
    #[test]
    fn error_body_yields_no_windows() {
        let body = json!({ "error": { "type": "forbidden", "message": "Request not allowed" } });
        assert!(normalize_claude_windows(&body).is_empty());
    }

    /// A 429 must be told apart from other failures: it backs off for minutes
    /// rather than seconds, because re-asking cannot succeed and only spends
    /// more of the budget the endpoint shares with Claude Code.
    #[test]
    fn a_rate_limit_is_distinguished_from_other_failures() {
        assert!(is_rate_limited("ohyear09@gmail.com: HTTP 429 Rate limited. Please try again later."));
        assert!(is_rate_limited("acct: HTTP 429 Too Many Requests"));
        assert!(!is_rate_limited("acct: HTTP 403 Request not allowed (region-blocked upstream)"));
        assert!(!is_rate_limited("acct: request to api.anthropic.com failed: timed out"));
        assert!(!is_rate_limited("acct: no OAuth token in the local secret store"));
    }

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// 2026-08-17T07:24:19Z — before every reset instant used below, so a
    /// window is never skipped for having already rolled over.
    const NOW: i64 = 1_786_951_459;

    /// A real Plus account's header block: the primary window is the weekly
    /// one and the secondary is unset. Labelling the pair "5h" and "24h" — what
    /// the served-vs-cap estimate reports — would have named both wrong.
    #[test]
    fn codex_windows_are_named_by_their_own_duration() {
        let w = normalize_codex_headers(
            &headers(&[
                ("x-codex-plan-type", "plus"),
                ("x-codex-active-limit", "premium"),
                ("x-codex-primary-used-percent", "9"),
                ("x-codex-primary-window-minutes", "10080"),
                ("x-codex-primary-reset-at", "1786430385"),
                ("x-codex-primary-reset-after-seconds", "430172"),
                ("x-codex-secondary-used-percent", "0"),
                ("x-codex-secondary-window-minutes", "0"),
                ("x-codex-secondary-reset-at", ""),
            ]),
            1_786_000_213,
        );
        assert_eq!(w.len(), 1, "the secondary slot is unset, not a window at 0%");
        assert_eq!(w[0]["label"], "7d");
        assert_eq!(w[0]["used_percent"], 9.0);
        assert_eq!(w[0]["window_seconds"], 604_800);
        assert_eq!(w[0]["reset_at"], 1_786_430_385_i64);
    }

    /// Both slots in use, and only the relative reset available — the shape a
    /// 5h + weekly plan reports.
    #[test]
    fn codex_falls_back_to_the_relative_reset() {
        let w = normalize_codex_headers(
            &headers(&[
                ("x-codex-primary-used-percent", "42.5"),
                ("x-codex-primary-window-minutes", "300"),
                ("x-codex-primary-reset-after-seconds", "600"),
                ("x-codex-secondary-used-percent", "7"),
                ("x-codex-secondary-window-minutes", "10080"),
                ("x-codex-secondary-reset-after-seconds", "86400"),
            ]),
            1_000,
        );
        let labels: Vec<&str> = w.iter().map(|x| x["label"].as_str().unwrap()).collect();
        assert_eq!(labels, ["5h", "7d"]);
        assert_eq!(w[0]["used_percent"], 42.5);
        assert_eq!(w[0]["reset_at"], 1_600_i64);
        assert_eq!(w[1]["reset_at"], 87_400_i64);
    }

    /// A response with no `x-codex-*` block at all — what a 400 rejected before
    /// the rate-limit middleware looks like. It must not read as "0% used".
    #[test]
    fn codex_headerless_response_yields_no_windows() {
        assert!(normalize_codex_headers(&headers(&[("content-type", "application/json")]), 0).is_empty());
    }

    /// Whether the gate can read a window at all is decided by its key, so a
    /// normaliser that spells one wrong produces numbers the Limits page shows
    /// and the gate silently ignores. Every provider's account-wide key has to
    /// survive the round trip into [`asale_client_core::quota`].
    fn gate_of(windows: &[Value], now: i64) -> Option<asale_client_core::quota::QuotaGate> {
        asale_client_core::quota::gate_from_windows(windows, now)
    }

    /// Code Assist reports headroom as a *remaining* fraction, per model. The
    /// per-model buckets are scope blocks, and the account-wide window the gate
    /// needs is synthesised from the roomiest of them — an account whose Pro
    /// quota is spent can still sell Flash.
    #[test]
    fn gemini_buckets_become_scoped_windows_plus_one_account_wide() {
        let body = json!({ "buckets": [
            { "modelId": "gemini-3-pro", "remainingFraction": 0.0, "resetTime": "2026-08-18T00:00:00Z" },
            { "modelId": "gemini-3-flash", "remainingFraction": 0.75, "resetTime": "2026-08-18T00:00:00Z" },
        ]});
        let w = normalize_gemini_buckets(&body);
        let keys: Vec<&str> = w.iter().map(|x| x["key"].as_str().unwrap()).collect();
        assert_eq!(keys, ["ws_gemini-3-pro", "ws_gemini-3-flash", "1d"]);
        assert_eq!(w[0]["used_percent"], 100.0);
        assert_eq!(w[1]["used_percent"], 25.0);

        let gate = gate_of(&w, NOW).expect("the synthetic account-wide window feeds the gate");
        assert!(!gate.exhausted(), "Flash is still sellable");
        assert_eq!(gate.headroom, 0.75);
        // Pro is spent, and only Pro.
        assert!(gate.scope_block("gemini-3-pro").is_some());
        assert!(gate.scope_block("gemini-3-flash").is_none());
    }

    /// An account-wide bucket is used as-is rather than synthesised over.
    #[test]
    fn a_gemini_account_wide_bucket_is_not_duplicated() {
        let body = json!({ "buckets": [
            { "remainingFraction": 0.4, "resetTime": "2026-08-18T00:00:00Z" },
            { "modelId": "gemini-3-pro", "remainingFraction": 0.9 },
        ]});
        let w = normalize_gemini_buckets(&body);
        assert_eq!(w.iter().filter(|x| x["key"] == "1d").count(), 1);
        assert_eq!(gate_of(&w, NOW).unwrap().headroom, 0.4);
    }

    /// A body with no readable fraction must not read as a free window.
    #[test]
    fn gemini_without_fractions_yields_no_windows() {
        assert!(normalize_gemini_buckets(&json!({ "buckets": [{ "modelId": "x" }] })).is_empty());
        assert!(normalize_gemini_buckets(&json!({ "error": { "code": 403 } })).is_empty());
    }

    /// Kimi's newer shape: one row per scope, `all` being the subscription-wide
    /// one. Its numbers are request counts, which the gate reads as a fraction
    /// like any other.
    #[test]
    fn kimi_data_rows_split_account_wide_from_per_model() {
        let body = json!({ "data": [
            { "model_name": "all", "used": 620, "limit": 1000, "resetTime": "2026-08-24T00:00:00Z",
              "duration": 7, "timeUnit": "DAY" },
            { "model_name": "kimi-k2.7", "used": 300, "limit": 300, "duration": 5, "timeUnit": "HOUR",
              "reset_in": 3600 },
        ]});
        let w = normalize_kimi_usage(&body);
        assert_eq!(w[0]["key"], "7d");
        assert_eq!(w[0]["used_percent"], 62.0);
        assert_eq!(w[1]["key"], "ws_kimi-k2.7");
        assert_eq!(w[1]["window_seconds"], 18_000);

        let gate = gate_of(&w, NOW).unwrap();
        assert_eq!(gate.tightest, "7d");
        assert!(gate.scope_block("kimi-k2.7-turbo").is_some(), "a spent model window blocks that model");
    }

    /// Kimi's other shape: a `limits` array of `{window, detail}` pairs, with
    /// the used count left to be derived from what remains.
    #[test]
    fn kimi_limit_rows_derive_usage_from_the_remainder() {
        let body = json!({ "limits": [
            { "window": { "duration": 300, "timeUnit": "MINUTES" },
              "detail": { "limit": 400, "remaining": 100, "reset_in": 1800 } },
        ]});
        let w = normalize_kimi_usage(&body);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0]["key"], "5h");
        assert_eq!(w[0]["used_percent"], 75.0);
        assert_eq!(w[0]["window_seconds"], 18_000);
    }

    /// A row with nothing to divide by is dropped: 0/0 would report a free
    /// window on an account nobody can read.
    #[test]
    fn kimi_rows_without_a_limit_are_dropped() {
        assert!(normalize_kimi_usage(&json!({ "data": [{ "model_name": "all", "used": 5 }] })).is_empty());
        assert!(normalize_kimi_usage(&json!({ "data": [{ "limit": 0, "used": 0 }] })).is_empty());
        assert!(normalize_kimi_usage(&json!({ "message": "unauthorized" })).is_empty());
    }

    /// xAI's block, if it sends one. The weekly allowance is a window worth
    /// gating on; the per-minute burst limit beside it is not, and reading it
    /// as one would pull a *busy* account off the market.
    #[test]
    fn a_burst_limit_is_not_mistaken_for_a_spent_subscription() {
        let w = normalize_ratelimit_headers(
            &headers(&[
                ("x-ratelimit-limit-requests", "1000"),
                ("x-ratelimit-remaining-requests", "250"),
                ("x-ratelimit-reset-requests", "3d12h"),
                ("x-ratelimit-limit-tokens", "60"),
                ("x-ratelimit-remaining-tokens", "1"),
                ("x-ratelimit-reset-tokens", "6s"),
            ]),
            NOW,
        );
        assert_eq!(w.len(), 1, "the 6-second token window is a burst limit");
        assert_eq!(w[0]["used_percent"], 75.0);
        assert_eq!(w[0]["reset_at"], NOW + 3 * 86_400 + 12 * 3_600);
        // Three and a half days left is a weekly pool part-spent, not a
        // three-and-a-half-day window.
        assert_eq!(w[0]["key"], "7d");
        assert_eq!(w[0]["label"], "7d requests");
        assert_eq!(gate_of(&w, NOW).unwrap().headroom, 0.25);
    }

    /// The horizon a header reports shrinks all week; the window it belongs to
    /// does not. Rounding up onto the ladder is what keeps one window from
    /// being relabelled every hour.
    #[test]
    fn a_reset_horizon_is_rounded_up_to_a_window_vendors_actually_use() {
        assert_eq!(window_from_horizon(1_800), 3_600);
        assert_eq!(window_from_horizon(3_600), 3_600);
        assert_eq!(window_from_horizon(4_000), 18_000);
        assert_eq!(window_from_horizon(80_000), 86_400);
        assert_eq!(window_from_horizon(302_400), 604_800);
        // Nothing sends a wider one; a month is the last rung rather than a
        // number that grows without bound.
        assert_eq!(window_from_horizon(999 * 86_400), 30 * 86_400);
    }

    /// The three spellings of a reset field that are in circulation, none of
    /// which is distinguishable from the others by anything but its shape.
    #[test]
    fn a_reset_field_is_read_as_absolute_or_relative_by_magnitude() {
        // Unix instant.
        assert_eq!(parse_reset_field("1786951999", NOW), Some(1_786_951_999));
        // Seconds from now.
        assert_eq!(parse_reset_field("1800", NOW), Some(NOW + 1800));
        // Go-style durations.
        assert_eq!(parse_reset_field("6m0s", NOW), Some(NOW + 360));
        assert_eq!(parse_reset_field("1h30m", NOW), Some(NOW + 5400));
        assert_eq!(parse_reset_field("250ms", NOW), Some(NOW));
        assert_eq!(parse_reset_field("", NOW), None);
        assert_eq!(parse_reset_field("soon", NOW), None);
    }

    /// A response with no rate-limit block must not read as "0% used".
    #[test]
    fn ratelimit_headerless_response_yields_no_windows() {
        assert!(normalize_ratelimit_headers(&headers(&[("content-type", "application/json")]), NOW).is_empty());
        // Half a block is not a window either.
        assert!(normalize_ratelimit_headers(&headers(&[("x-ratelimit-limit-requests", "100")]), NOW).is_empty());
    }

    /// Which providers can be asked while idle, and which can only be listened
    /// to. Getting this wrong either wastes a request on an endpoint that does
    /// not exist or leaves a readable account on the local estimate.
    #[test]
    fn only_providers_with_a_free_endpoint_are_asked() {
        for p in ["claude", "claude_work", "gemini", "kimi"] {
            assert!(has_usage_endpoint(p), "{p} answers a free usage endpoint");
        }
        for p in ["codex", "xai", "xai_api", "kimi_api", "deepseek", "custom"] {
            assert!(!has_usage_endpoint(p), "{p} has nothing free to read");
        }
    }
}
