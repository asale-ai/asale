//! Keeping the sell-side gate fed with the provider's own rate-limit numbers.
//!
//! Before this loop existed the readings were pulled only by the Limits page,
//! which meant a device that sells around the clock and is never looked at had
//! no reading at all — and the gate fell back to
//! [`asale_client_core::discovery::estimate_quota_window`], a guessed plan cap
//! minus this device's own sales. On a Claude login (whose OAuth exchange
//! carries no plan, so the cap lands on the lowest paid tier) that stopped a
//! live subscription after ~220k tokens and left it "spent" for five hours
//! while the real five-hour window sat at 3%.
//!
//! # What is polled, and what is not
//!
//! Claude publishes `oauth/usage`, a read that costs no subscription quota, so
//! its accounts are asked here on a slow clock. Codex publishes nothing a
//! ChatGPT bearer may read — its numbers only ride back on the headers of an
//! accepted API call — so its readings keep coming from the serving path
//! (`RecordSink::observe_quota`), which banks one per sale, per account. An
//! idle Codex account gets no fresh reading and needs none: a window nobody is
//! spending does not move. Buying one with a probe is what the Limits page does
//! when a person is actually looking.
//!
//! # Per account, never per family
//!
//! [`commands::usage::live_windows`] stops at the first account that answers,
//! because the page shows one row per provider. The gate cannot: two Claude
//! logins may be two different subscriptions, and letting one account's reading
//! decide another's would take a healthy subscription off the market because a
//! different one is spent. So every sell-enabled account is asked for itself.

use crate::commands::usage::{account_quota_gate, fetch_claude_windows, record_quota_windows};
use crate::keychain;
use crate::publisher;
use crate::state::AppState;
use std::sync::Arc;
use std::time::Duration;

/// How often the readings are refreshed.
///
/// Five minutes rather than one: `oauth/usage` shares Claude Code's own request
/// budget and answers 429 on a busy account, and the windows it describes are
/// hours to days wide. The reset instants below are what make a slow clock
/// safe — a window due back inside the next tick is waited for exactly, not
/// polled over.
const POLL_SECS: i64 = 300;

/// Slack added to a reset instant before re-asking. The vendor's clock and ours
/// are not the same clock, and asking a second early buys a reading that still
/// says 100%.
const RESET_SLACK_SECS: i64 = 10;

/// How often the plan behind an account is re-read from `oauth/profile`.
///
/// Six hours, because a plan changes when somebody upgrades and not otherwise,
/// and because the first sweep after start-up already fixes every account that
/// connected before this existed. An account with no plan at all is asked every
/// sweep instead: it is the case that costs a Max subscription nineteen
/// twentieths of its capacity, so it is worth one request per five minutes
/// until it is answered.
const PLAN_TTL_SECS: i64 = 6 * 3600;

/// Settings key recording when the plan above was last read.
fn plan_at_key(provider: &str, account_id: &str) -> String {
    format!("plan_at:{provider}:{account_id}")
}

/// Providers with a usage endpoint that can be read without spending quota.
fn pollable(provider: &str) -> bool {
    matches!(provider, "claude" | "claude_work")
}

/// When to wake next: the ordinary tick, or sooner if a spent window is due
/// back before it.
///
/// Split out as a pure function because it is the whole scheduling policy — the
/// loop around it is a `sleep` and a `for`.
fn next_delay(now: i64, resets: &[i64]) -> Duration {
    let soonest = resets
        .iter()
        .filter(|t| **t > now)
        .min()
        .map(|t| t - now + RESET_SLACK_SECS)
        .unwrap_or(POLL_SECS);
    Duration::from_secs(soonest.clamp(30, POLL_SECS) as u64)
}

/// Re-read which plan this account is on, if it is time to.
///
/// Separate from the quota reading because the two age at completely different
/// rates — utilisation moves every minute, a plan moves when somebody upgrades
/// — and because this is the half that fixes the accounts already connected
/// when this shipped. Those have no plan stored at all: Anthropic's token
/// exchange never carried one, so `plan_window_cap` sized them as the lowest
/// paid tier and a Max 20× subscription sold 220k tokens per five hours instead
/// of 4.4M.
///
/// Returns true when the stored plan changed, which is a re-gate: the cap it
/// picks is what converts the provider's utilisation into the headroom this
/// device declares.
async fn refresh_plan(state: &AppState, provider: &str, account_id: &str, now: i64) -> bool {
    let key = format!("plan:{provider}:{account_id}");
    let known = state.store.get_setting(&key).await.ok().flatten().filter(|s| !s.is_empty());
    if known.is_some() {
        let last: i64 = state
            .store
            .get_setting(&plan_at_key(provider, account_id))
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if now - last < PLAN_TTL_SECS {
            return false;
        }
    }
    let Some(token) = keychain::get(&keychain::token_ref(provider, account_id)).ok().flatten() else {
        return false;
    };
    let plan = match asale_client_core::discovery::fetch_claude_profile(&token).await {
        Ok(body) => asale_client_core::discovery::claude_plan_from_profile(&body),
        Err(e) => {
            tracing::debug!(provider, account = %account_id, "oauth/profile lookup failed: {e}");
            return false;
        }
    };
    // Stamp the attempt either way: an account the profile answers *without* a
    // plan must not be re-asked every five minutes forever.
    let _ = state.store.set_setting(&plan_at_key(provider, account_id), &now.to_string()).await;
    let Some(plan) = plan else { return false };
    if known.as_deref() == Some(plan.as_str()) {
        return false;
    }
    tracing::info!(
        provider, account = %account_id, was = ?known, now = %plan,
        "plan read from the provider's profile — subscription capacity re-sized"
    );
    let _ = state.store.set_setting(&key, &plan).await;
    true
}

/// Refresh one account's reading and bank it. Errors are the caller's to log:
/// a 429 or a region block is expected often enough that it must not be noise.
///
/// `share_with_page` hands the reading to the Limits page's per-provider cache
/// as well. Only the family's first answer does, matching what
/// `claude_windows_cached` would have stored — and it is what keeps a page poll
/// from spending a second request on `oauth/usage`, which shares Claude Code's
/// own budget and 429s for minutes at a time when it is leaned on.
async fn refresh_account(
    state: &AppState,
    provider: &str,
    account_id: &str,
    share_with_page: bool,
) -> Result<(), String> {
    let token = keychain::get(&keychain::token_ref(provider, account_id))
        .ok()
        .flatten()
        .ok_or_else(|| "no OAuth token in the local secret store".to_string())?;
    let windows = fetch_claude_windows(&token).await.map_err(|e| e.to_string())?;
    let windows = serde_json::Value::Array(windows);
    record_quota_windows(&state.store, provider, account_id, &windows).await;
    if share_with_page {
        let now = crate::commands::now_secs();
        state.limits_cache.write().await.insert(provider.to_string(), (now, Ok(windows)));
    }
    Ok(())
}

/// Poll the readings, re-gate the pool, and push the result to the market.
pub fn spawn_quota_loop(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let now = crate::commands::now_secs();
            let tools = state.store.list_tools().await.unwrap_or_default();
            // Which plan each account is on, before how much of it is left: the
            // plan picks the cap the utilisation is applied to, so reading them
            // the other way round would gate a whole sweep on a stale one.
            //
            // Every connected account, not only the selling ones — the Limits
            // page sizes a switched-off subscription too, and this is a free
            // endpoint asked twice a day.
            let mut replan = false;
            for tool in tools.iter().filter(|t| pollable(&t.provider)) {
                replan |= refresh_plan(&state, &tool.provider, &tool.account_id, now).await;
            }

            let mut asked = 0usize;
            let mut shared: std::collections::BTreeSet<String> = Default::default();
            for tool in tools.iter().filter(|t| t.sell_enabled && pollable(&t.provider)) {
                asked += 1;
                let first_of_family = !shared.contains(&tool.provider);
                match refresh_account(&state, &tool.provider, &tool.account_id, first_of_family).await {
                    Ok(()) => {
                        shared.insert(tool.provider.clone());
                    }
                    // Debug, not warn: an account that is rate-limited or
                    // geo-blocked hits this every five minutes, and the gate
                    // degrades to the local estimate rather than breaking.
                    Err(e) => tracing::debug!(
                        provider = %tool.provider, account = %tool.account_id,
                        "quota poll: {e}"
                    ),
                }
            }

            // A re-sized plan changes the gate even on a sweep where no reading
            // could be refreshed, so it earns a rebuild of its own.
            if asked > 0 || replan {
                // What the gate said before the readings landed, so a lane
                // coming back (or leaving) is pushed to the market now rather
                // than at the recovery loop's next minute.
                let before = selling_lanes(&state);
                publisher::rebuild_pool(&state.store, &state.pool).await;
                let after = selling_lanes(&state);
                if before != after {
                    tracing::info!(
                        was = before.len(), now = after.len(),
                        "quota gate changed what this device is offering"
                    );
                    if let Some(h) = state.publisher.read().await.as_ref() {
                        h.nudge();
                    }
                }
            }

            // Every reset instant the gate is currently waiting on, so a window
            // due back in 40 seconds is re-read then and not five minutes later.
            let mut resets = Vec::new();
            for tool in tools.iter().filter(|t| t.sell_enabled) {
                if let Some((g, _)) = account_quota_gate(&state.store, &tool.provider, &tool.account_id, now).await {
                    resets.extend(g.reset_at);
                    resets.extend(g.blocked.iter().filter_map(|b| b.reset_at));
                }
            }
            tokio::time::sleep(next_delay(now, &resets)).await;
        }
    })
}

/// The `(provider, account, model)` lanes currently selling.
fn selling_lanes(state: &AppState) -> std::collections::BTreeSet<(String, String, String)> {
    let now = crate::commands::now_secs();
    state
        .pool
        .lock()
        .map(|p| {
            p.lane_views(now)
                .into_iter()
                .filter(|v| v.status == "selling")
                .map(|v| (v.provider, v.account_id, v.model))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_786_951_459;

    #[test]
    fn an_idle_gate_waits_the_full_tick() {
        assert_eq!(next_delay(NOW, &[]), Duration::from_secs(POLL_SECS as u64));
        // Resets already in the past are history, not a reason to wake early.
        assert_eq!(next_delay(NOW, &[NOW - 5, NOW - 900]), Duration::from_secs(POLL_SECS as u64));
    }

    #[test]
    fn a_window_due_back_soon_is_waited_for_exactly() {
        // 100s out, plus the slack that keeps us from asking a second early.
        assert_eq!(next_delay(NOW, &[NOW + 100]), Duration::from_secs(110));
        // The soonest one wins.
        assert_eq!(next_delay(NOW, &[NOW + 250, NOW + 100, NOW + 4000]), Duration::from_secs(110));
    }

    /// A reset in a few seconds must not turn into a spin: `oauth/usage` is a
    /// shared budget, and a lane coming back a few seconds late costs nothing.
    #[test]
    fn a_reset_that_has_all_but_arrived_still_waits() {
        assert_eq!(next_delay(NOW, &[NOW + 1]), Duration::from_secs(30));
    }

    /// A window hours out does not push the ordinary tick out with it.
    #[test]
    fn a_distant_reset_does_not_stretch_the_poll() {
        assert_eq!(next_delay(NOW, &[NOW + 86_400]), Duration::from_secs(POLL_SECS as u64));
    }

    #[test]
    fn only_providers_with_a_free_read_are_polled() {
        assert!(pollable("claude"));
        assert!(pollable("claude_work"));
        // Codex's numbers ride on the serving path; probing costs quota.
        assert!(!pollable("codex"));
        assert!(!pollable("custom"));
    }
}
