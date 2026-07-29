//! Usage and rate-limit views: what this device sold, bought and used, and
//! how much of each provider's subscription window is left.

use crate::keychain;
use crate::state::AppState;
use asale_client_core::store::ToolRow;
use asale_client_core::{discovery, Provider};
use serde_json::{json, Value};
use super::{R, day_start_ts, day_str, err, now_secs};
use crate::cmd_err;

/// Rolling window used for the subscription capacity estimate (5h, mirrors
/// `publisher::WINDOW_SECS`).
pub(crate) const WINDOW_SECS: i64 = 5 * 3600;

/// Three-category token usage summary over a period (day / week / month / all):
///   - `bought` — tokens consumed from the market (`consume_records`).
///   - `sold`   — tokens served as a provider (`provider_records`).
/// The subscription category is derived by the UI from `publish_limits` +
/// `list_accounts` (capacity vs. window usage), so it is not repeated here.
pub async fn usage_summary(state: &AppState, period: String) -> R<Value> {
    // Cutoff in unix seconds; `None` = all-time.
    let since: Option<i64> = match period.as_str() {
        "day" => Some(day_start_ts()),
        "week" => Some(now_secs() - 7 * 86400),
        "month" => Some(now_secs() - 30 * 86400),
        _ => None, // "all"
    };
    let sold = state.store.sold_summary(since).await.map_err(err)?;
    let bought = state.store.bought_summary(since).await.map_err(err)?;
    Ok(json!({
        "period": period,
        "sold":   { "tokens": sold.0,   "amount_usdt": sold.1,   "count": sold.2 },
        "bought": { "tokens": bought.0, "amount_usdt": bought.1, "count": bought.2 },
    }))
}

/// Full usage dashboard for the Usage page (mirrors TokenTracker's overview):
/// headline totals, per-model breakdown, a per-day table, a heatmap series and
/// rolling stats. `scope` picks the record source — `bought` (consume_records),
/// `sold` (provider_records) or `used` (local CLI logs). `period` bounds the
/// headline / table / model breakdown; the heatmap spans the last ~150 days.
pub async fn usage_overview(state: &AppState, period: String, scope: String) -> R<Value> {
    // Fold any newly-inserted ledger rows into the snapshot first (incremental —
    // only rows added since the last run are scanned), then read the snapshot.
    state.store.aggregate_usage().await.map_err(err)?;
    // Fold local AI-CLI session logs so "我使用的" reflects current usage.
    let _ = crate::usage_scan::scan_claude_logs(&state.store).await;

    let sources: &[&str] = match scope.as_str() {
        "bought" => &["bought"],
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

/// Per-provider subscription capacity summed across connected accounts:
/// `provider -> (window_cap tokens over 5h, account count, representative plan)`.
pub(crate) async fn provider_caps(state: &AppState) -> R<std::collections::HashMap<String, (u64, i64, Option<String>)>> {
    let tools = state.store.list_tools().await.map_err(err)?;
    let mut caps: std::collections::HashMap<String, (u64, i64, Option<String>)> = std::collections::HashMap::new();
    for tool in &tools {
        let Some(prov) = Provider::from_str_opt(&tool.provider) else { continue };
        // Prefer the plan captured at OAuth/import time, else the tools column.
        let plan = state
            .store
            .get_setting(&format!("plan:{}:{}", tool.provider, tool.account_id))
            .await
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .or_else(|| tool.plan.clone());
        let cap = discovery::plan_window_cap(prov, plan.as_deref());
        let entry = caps.entry(tool.provider.clone()).or_insert((0, 0, None));
        entry.0 += cap;
        entry.1 += 1;
        if entry.2.is_none() {
            entry.2 = plan;
        }
    }
    Ok(caps)
}

/// Rate-limit / quota windows per provider for the Limits page (mirrors
/// TokenTracker's usage-limits payload). Each provider reports whether it is
/// connected, a plan label, and a list of windows — 5h rolling + daily — with
/// `used_percent`, an absolute `reset_at` (unix seconds), and `window_seconds`
/// so the UI can compute pace markers and reset countdowns.
///
/// `live` says whether those windows are the provider's own numbers, and
/// `fallback_reason` says why they are not when a live read was attempted and
/// failed. Without that second field a geo-blocked upstream (403 "Request not
/// allowed" — the usual state when no upstream proxy is configured) silently
/// degrades to a served-vs-cap estimate that looks like real data and hides
/// whole windows, e.g. the weekly per-model ones.
pub async fn usage_limits(state: &AppState, force: Option<bool>) -> R<Value> {
    let force = force.unwrap_or(false);
    let caps = provider_caps(state).await?;
    let tools = state.store.list_tools().await.map_err(err)?;
    let mut providers = Vec::new();
    for provider in asale_protocol::ids::SUBSCRIBABLE_PROVIDERS.iter().map(|p| p.as_str()) {
        let (window_cap, accounts, plan) = caps.get(provider).cloned().unwrap_or((0, 0, None));
        if accounts == 0 {
            providers.push(json!({ "id": provider, "connected": false }));
            continue;
        }

        // Prefer the provider's *real* rate-limit windows (Claude oauth/usage),
        // cached for 60s. Fall back to the local served-vs-cap estimate.
        let (windows, live, fallback_reason) = match claude_windows_cached(state, provider, &tools, force).await {
            LiveWindows::Ok(w) => (w, true, None),
            other => {
                let est = estimate_windows(state, provider, window_cap).await?;
                let reason = match other {
                    LiveWindows::Failed(e) => Some(e),
                    _ => None, // provider has no usage endpoint we can read
                };
                (est, false, reason)
            }
        };

        providers.push(json!({
            "id": provider,
            "connected": true,
            "plan_label": plan,
            "windows": windows,
            "live": live,
            "fallback_reason": fallback_reason,
        }));
    }
    Ok(json!({ "providers": providers }))
}

/// Local stand-in for a provider that has no readable usage endpoint (or whose
/// endpoint we could not reach): what *this device* served against the plan's
/// capacity. It measures asale's own selling only, so it is an estimate — the
/// caller must label it as one.
async fn estimate_windows(state: &AppState, provider: &str, window_cap: u64) -> R<Value> {
    let prefix = provider_model_prefix(provider);
    let window_cap = window_cap as f64;
    let daily_cap = window_cap * 24.0 / 5.0;
    let used_window = state.store.served_tokens_since(WINDOW_SECS, prefix).await.map_err(err)? as f64;
    let used_today = state.store.served_tokens_today(prefix).await.map_err(err)? as f64;
    let oldest = state.store.oldest_served_ts_since(WINDOW_SECS, prefix).await.map_err(err)?;
    let win_pct = if window_cap > 0.0 { (used_window / window_cap * 100.0).min(100.0) } else { 0.0 };
    let day_pct = if daily_cap > 0.0 { (used_today / daily_cap * 100.0).min(100.0) } else { 0.0 };
    let win_reset = oldest.map(|t| t + WINDOW_SECS);
    Ok(json!([
        { "key": "5h", "label": "5h", "used_percent": win_pct, "reset_at": win_reset, "window_seconds": WINDOW_SECS },
        { "key": "1d", "label": "24h", "used_percent": day_pct, "reset_at": day_start_ts() + 86400, "window_seconds": 86400 },
    ]))
}

/// Outcome of a live rate-limit read.
pub(crate) enum LiveWindows {
    /// The provider's own windows (a JSON array of LimitWindow).
    Ok(Value),
    /// This provider exposes no usage endpoint we can read.
    Unsupported,
    /// A read was attempted and failed; the string says why, verbatim enough to
    /// act on (HTTP status + upstream message, or the transport error).
    Failed(String),
}

/// Live Claude rate-limit windows via `api.anthropic.com/api/oauth/usage`,
/// cached per provider (60s on success — the upstream endpoint shares Claude
/// Code's budget and 429s easily; 30s on failure, so a page poll every 30s does
/// not queue up 12-second timeouts). `force` bypasses the cache.
pub(crate) async fn claude_windows_cached(
    state: &AppState,
    provider: &str,
    tools: &[ToolRow],
    force: bool,
) -> LiveWindows {
    if provider != "claude" && provider != "claude_work" {
        return LiveWindows::Unsupported;
    }
    const TTL: i64 = 60;
    const FAIL_TTL: i64 = 30;
    let now = now_secs();
    if !force {
        let cache = state.limits_cache.read().await;
        if let Some((at, cached)) = cache.get(provider) {
            let fresh = now - at < if cached.is_ok() { TTL } else { FAIL_TTL };
            if fresh {
                return match cached {
                    Ok(w) => LiveWindows::Ok(w.clone()),
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
        match fetch_claude_windows(&token).await {
            Ok(w) => {
                let v = Value::Array(w);
                state.limits_cache.write().await.insert(provider.to_string(), (now, Ok(v.clone())));
                return LiveWindows::Ok(v);
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

/// LIKE-prefix used to attribute local publisher usage to a provider family
/// (mirrors `publisher::provider_model_prefix`).
pub(crate) fn provider_model_prefix(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" | "claude_work" => Some("claude"),
        "gemini" => Some("gemini"),
        "codex" => Some("gpt"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
