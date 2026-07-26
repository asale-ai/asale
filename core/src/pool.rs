//! Multi-account pool (spec §4, modeled on CLIProxyAPI selector/cooldown).
//! Holds one `AccountRuntime` per imported account; the executor and the
//! consumer proxy pick accounts through it, and report task outcomes back so
//! rate-limited / failing accounts cool down instead of being hammered.
//!
//! # Lanes
//!
//! Availability is tracked per **lane** — one `(account, model)` pair — not per
//! account. One subscription sells several models and they fail independently:
//! an Opus window can be spent while Haiku is wide open, and a model can be
//! rate-limited on its own. Pausing the whole account for that took capacity
//! off the market that was never in trouble.
//!
//! # Why the client owns this
//!
//! The gateway sees relayed error codes; this process sees the upstream status,
//! the `Retry-After`, the token expiry and the real remaining quota. So the
//! decision to stop selling a lane — and the decision that it is safe to start
//! again — is made here, and the server is simply told, via a re-declaration
//! (`PublisherHandle::nudge`). Every transition below is expected to be
//! followed by a nudge; see `daemon/src/publisher.rs`.
//!
//! # Recovery ladder (spec §4.5)
//!
//! Isolated failures are transient and self-heal; a lane that fails over and
//! over is broken and needs a person. So consecutive failures escalate:
//!
//! | consecutive transient failures | result                       |
//! |--------------------------------|------------------------------|
//! | 1                              | 30s cooldown, auto-resume    |
//! | 2                              | 2m cooldown, auto-resume     |
//! | ≥3                             | paused, **operator resumes** |
//!
//! Rate limits and spent quota are not faults: they pause the lane with a
//! `resume_at` and come back on their own. A failed auth needs the operator to
//! sign in again, so it is manual from the first occurrence.

use serde::Serialize;
use std::collections::BTreeMap;

/// Default cooldown after a transient upstream failure (5xx), seconds.
pub const COOLDOWN_TRANSIENT_SECS: i64 = 300;
/// Cooldown after a rate-limit (429) when the upstream gives no reset, seconds.
/// Rate limits are window-scoped, so cool longer than plain transient errors.
pub const COOLDOWN_RATE_LIMIT_SECS: i64 = 900;
/// Auto-recovery cooldowns for the first transient failures of a lane, seconds.
/// Running out of rungs trips the breaker.
pub const LANE_COOLDOWN_LADDER: [i64; 2] = [30, 120];
/// Consecutive transient failures that pause a lane for good (until resumed) —
/// one past the last rung, so the two cannot drift apart.
pub const LANE_BREAKER_THRESHOLD: u32 = LANE_COOLDOWN_LADDER.len() as u32 + 1;

/// Why a lane is not selling right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// Upstream 429. Comes back at `resume_at`.
    RateLimit,
    /// Rolling window or daily sell cap spent. Comes back at the rollover.
    Quota,
    /// 401/403 — the operator has to sign in again.
    Auth,
    /// Too many consecutive failures; the operator has to look at it.
    Breaker,
    /// The operator switched this lane off.
    Manual,
    /// The platform does not trade this model (no price row, or an operator
    /// disabled it). Nothing the seller can fix locally, and nothing to wait
    /// out either — it clears when the catalog lists the model again.
    Untradable,
}

impl PauseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            PauseReason::RateLimit => "rate_limit",
            PauseReason::Quota => "quota",
            PauseReason::Auth => "auth",
            PauseReason::Breaker => "breaker",
            PauseReason::Manual => "manual",
            PauseReason::Untradable => "untradable",
        }
    }

    pub fn parse(s: &str) -> Option<PauseReason> {
        Some(match s {
            "rate_limit" => PauseReason::RateLimit,
            "quota" => PauseReason::Quota,
            "auth" => PauseReason::Auth,
            "breaker" => PauseReason::Breaker,
            "manual" => PauseReason::Manual,
            "untradable" => PauseReason::Untradable,
            _ => return None,
        })
    }

    /// Whether clearing this needs a person. The UI shows a "resume selling"
    /// button exactly for these; the rest disappear on their own, and offering
    /// a button for them would only invite the operator to fight a countdown.
    pub fn requires_user(self) -> bool {
        matches!(self, PauseReason::Auth | PauseReason::Breaker | PauseReason::Manual)
    }
}

/// Per-`(account, model)` serving state.
#[derive(Debug, Clone, Default)]
pub struct LaneState {
    /// Auto-clearing exclusion (ladder rungs, rate-limit resets).
    pub cooldown_until: Option<i64>,
    /// Consecutive transient failures; any success resets it.
    pub fail_streak: u32,
    /// Set when the lane is out until something specific happens.
    pub paused: Option<PauseReason>,
    /// When a `paused` lane expects to return; 0 = needs the operator.
    pub resume_at: i64,
    /// Last upstream error, for the UI.
    pub last_error: String,
}

impl LaneState {
    /// Whether this lane may serve market traffic at `now`.
    pub fn servable(&self, now: i64) -> bool {
        self.paused.is_none() && !self.cooldown_until.is_some_and(|c| c > now)
    }

    /// The next instant at which this lane's state changes by itself, if any.
    /// Drives the client's wake-up timers: without them a lane that is ready
    /// again waits for the periodic re-declaration to notice.
    pub fn auto_resume_at(&self, now: i64) -> Option<i64> {
        let cd = self.cooldown_until.filter(|c| *c > now);
        let pause = match self.paused {
            Some(r) if !r.requires_user() && self.resume_at > now => Some(self.resume_at),
            _ => None,
        };
        match (cd, pause) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Why an upstream call failed, as far as account selection cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamErrorKind {
    /// 429 — cool down until the given reset (or the rate-limit default).
    RateLimited { reset_at: Option<i64> },
    /// 5xx / transport error — short cooldown.
    ServerError,
    /// 401/403 — the token is bad; keep the account out until it is refreshed.
    AuthFailed,
}

/// Selection strategy (spec §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Rotate through available accounts (least-recently-used first).
    RoundRobin,
    /// Drain the account with the most remaining quota first.
    FillFirst,
}

/// Runtime state of one account inside the pool.
#[derive(Debug, Clone)]
pub struct AccountRuntime {
    pub provider: String,
    pub account_id: String,
    /// Keychain reference for the access token (never the token itself).
    pub keychain_ref: String,
    pub plan: Option<String>,
    /// Estimated serviceable tokens remaining in the account's window.
    pub quota_remaining: u64,
    /// Access-token expiry (unix secs) when known.
    pub expires_at: Option<i64>,
    /// Per-account sell switch. Only sell-enabled accounts are handed to the
    /// relay executor; the local consumer proxy's `direct` route ignores it
    /// (using your own subscription locally is not a sale).
    pub sell_enabled: bool,
    /// Where the credential came from — `oauth` (asale-owned) or `import`
    /// (shared with a locally installed CLI). Display/warning only.
    pub origin: Option<String>,
    /// Tokens this account already served today (UTC), for the daily cap UI.
    pub used_today: u64,
    /// Per-account daily sell cap in tokens; 0 = unlimited.
    pub sell_daily_limit: i64,
    pub cooldown_until: Option<i64>,
    /// Set on 401/403; cleared when a rebuild sees a newer expiry (refreshed).
    pub auth_failed: bool,
    pub last_used: i64,
    pub in_use: u32,
    pub concurrency_max: u32,
    /// Per-model serving state, keyed by model id. The key set is this
    /// account's model catalogue: a model with no entry here is not sold.
    pub lanes: BTreeMap<String, LaneState>,
}

impl AccountRuntime {
    pub fn new(provider: &str, account_id: &str, keychain_ref: &str) -> AccountRuntime {
        AccountRuntime {
            provider: provider.to_string(),
            account_id: account_id.to_string(),
            keychain_ref: keychain_ref.to_string(),
            plan: None,
            quota_remaining: 0,
            expires_at: None,
            sell_enabled: false,
            origin: None,
            used_today: 0,
            sell_daily_limit: 0,
            cooldown_until: None,
            auth_failed: false,
            last_used: 0,
            in_use: 0,
            concurrency_max: 4,
            lanes: BTreeMap::new(),
        }
    }

    /// Declare which models this account sells. Existing lanes keep their
    /// runtime state; models no longer in the catalogue are dropped.
    pub fn with_models<I, S>(mut self, models: I) -> AccountRuntime
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.lanes = models
            .into_iter()
            .map(|m| (m.as_ref().to_string(), LaneState::default()))
            .collect();
        self
    }

    /// UI/state label: available | cooldown | expired | exhausted.
    pub fn status(&self, now: i64) -> &'static str {
        if self.auth_failed || self.expires_at.is_some_and(|e| e <= now) {
            "expired"
        } else if self.cooldown_until.is_some_and(|c| c > now) {
            "cooldown"
        } else if self.quota_remaining == 0 {
            "exhausted"
        } else {
            "available"
        }
    }

    fn available(&self, now: i64) -> bool {
        self.status(now) == "available" && self.in_use < self.concurrency_max
    }

    /// Whether this account may *sell* one model right now: the account has to
    /// be healthy and the lane neither cooling nor paused.
    fn lane_available(&self, model: &str, now: i64) -> bool {
        self.available(now) && self.lanes.get(model).is_some_and(|l| l.servable(now))
    }

    /// Whether this account may serve one model *locally*.
    ///
    /// A cooling lane is unhealthy for everyone, so local traffic backs off
    /// too. A *pause* is a market decision — "not for sale" — and must not lock
    /// the operator out of the subscription they are paying for.
    fn lane_usable(&self, model: &str, now: i64) -> bool {
        self.available(now)
            && self
                .lanes
                .get(model)
                .is_none_or(|l| !l.cooldown_until.is_some_and(|c| c > now))
    }
}

/// A pick result — enough to fetch the token and to report the outcome later.
#[derive(Debug, Clone)]
pub struct PickedAccount {
    pub provider: String,
    pub account_id: String,
    pub keychain_ref: String,
}

/// Serializable status view for `list_accounts`.
#[derive(Debug, Clone, Serialize)]
pub struct AccountStatusView {
    pub provider: String,
    pub account_id: String,
    pub plan: Option<String>,
    pub quota_remaining: u64,
    pub status: String,
    pub expires_at: Option<i64>,
    pub cooldown_until: Option<i64>,
    pub sell_enabled: bool,
    pub origin: Option<String>,
    pub used_today: u64,
    pub sell_daily_limit: i64,
}

/// One `(account, model)` lane, as the UI and the declaration builder see it.
#[derive(Debug, Clone, Serialize)]
pub struct LaneStatusView {
    pub provider: String,
    pub account_id: String,
    pub model: String,
    /// selling | cooldown | paused | off | expired | exhausted
    pub status: String,
    pub paused_reason: Option<String>,
    /// True when the operator has to act before this lane sells again.
    pub requires_user: bool,
    /// Unix seconds it is expected back; 0 when unknown or operator-gated.
    pub resume_at: i64,
    pub cooldown_until: Option<i64>,
    pub fail_streak: u32,
    pub last_error: String,
    pub sell_enabled: bool,
    pub quota_remaining: u64,
}

pub struct AccountPool {
    strategy: Strategy,
    accounts: Vec<AccountRuntime>,
}

impl AccountPool {
    pub fn new(strategy: Strategy) -> AccountPool {
        AccountPool { strategy, accounts: Vec::new() }
    }

    /// Replace the account set (from the store), preserving live runtime state
    /// (cooldown, usage, last_used) for accounts that persist. `auth_failed` is
    /// cleared when the incoming expiry differs — that means the token was
    /// refreshed since the failure.
    pub fn set_accounts(&mut self, fresh: Vec<AccountRuntime>) {
        let old = std::mem::take(&mut self.accounts);
        self.accounts = fresh
            .into_iter()
            .map(|mut a| {
                if let Some(prev) = old
                    .iter()
                    .find(|o| o.provider == a.provider && o.account_id == a.account_id)
                {
                    a.cooldown_until = prev.cooldown_until;
                    a.last_used = prev.last_used;
                    a.in_use = prev.in_use;
                    let refreshed = prev.expires_at != a.expires_at;
                    if !refreshed {
                        a.auth_failed = prev.auth_failed;
                    }
                    // Lane state is runtime state: a rebuild (which happens
                    // every minute, and on every account edit) must not forget
                    // that a lane is cooling or broken. Only lanes still in the
                    // incoming catalogue carry over.
                    for (model, lane) in &mut a.lanes {
                        let Some(prev_lane) = prev.lanes.get(model) else { continue };
                        *lane = prev_lane.clone();
                        // A refreshed token is exactly the recovery signal an
                        // auth pause was waiting for.
                        if refreshed && lane.paused == Some(PauseReason::Auth) {
                            *lane = LaneState::default();
                        }
                        // The incoming catalogue is built from what the
                        // platform trades, so a lane still in it is traded
                        // again — and nothing else would ever clear this
                        // pause: it has no resume instant and no operator
                        // button, by design.
                        if lane.paused == Some(PauseReason::Untradable) {
                            *lane = LaneState::default();
                        }
                    }
                }
                a
            })
            .collect();
    }

    /// Pick an available account for `provider` and lease one concurrency slot.
    /// Used by the local consumer proxy's `direct` route — running your own
    /// subscription locally is not a sale, so neither the sell switch nor the
    /// lane pauses (which are about selling) apply.
    pub fn pick(&mut self, provider: &str, now: i64) -> Option<PickedAccount> {
        self.pick_where(provider, None, now, false)
    }

    /// Pick an account for local (non-sale) use of one model. Honours the
    /// lane's cooldown — a lane that just failed is unhealthy for local traffic
    /// too — but not its market pauses.
    pub fn pick_local(&mut self, provider: &str, model: &str, now: i64) -> Option<PickedAccount> {
        self.pick_where(provider, Some(model), now, false)
    }

    /// Pick an account that is switched on for selling *and* whose lane for
    /// this model is serving — the only accounts the relay executor may serve
    /// market traffic from.
    pub fn pick_for_sale(&mut self, provider: &str, model: &str, now: i64) -> Option<PickedAccount> {
        self.pick_where(provider, Some(model), now, true)
    }

    fn pick_where(&mut self, provider: &str, model: Option<&str>, now: i64, sell_only: bool) -> Option<PickedAccount> {
        let mut best: Option<usize> = None;
        for (i, a) in self.accounts.iter().enumerate() {
            if a.provider != provider || (sell_only && !a.sell_enabled) {
                continue;
            }
            let ok = match (model, sell_only) {
                (Some(m), true) => a.lane_available(m, now),
                (Some(m), false) => a.lane_usable(m, now),
                (None, _) => a.available(now),
            };
            if !ok {
                continue;
            }
            best = Some(match best {
                None => i,
                Some(b) => {
                    let cur = &self.accounts[b];
                    let better = match self.strategy {
                        // Least-recently-used first; tie → more quota remaining.
                        Strategy::RoundRobin => {
                            a.last_used < cur.last_used
                                || (a.last_used == cur.last_used && a.quota_remaining > cur.quota_remaining)
                        }
                        // Most quota first; tie → least recently used.
                        Strategy::FillFirst => {
                            a.quota_remaining > cur.quota_remaining
                                || (a.quota_remaining == cur.quota_remaining && a.last_used < cur.last_used)
                        }
                    };
                    if better {
                        i
                    } else {
                        b
                    }
                }
            });
        }
        let i = best?;
        let a = &mut self.accounts[i];
        a.last_used = now;
        a.in_use += 1;
        Some(PickedAccount {
            provider: a.provider.clone(),
            account_id: a.account_id.clone(),
            keychain_ref: a.keychain_ref.clone(),
        })
    }

    /// Whether any account could serve `provider` right now (no lease taken).
    pub fn any_available(&self, provider: &str, now: i64) -> bool {
        self.accounts.iter().any(|a| a.provider == provider && a.available(now))
    }

    /// Whether any *sell-enabled* account could serve this lane right now —
    /// what the supply declaration must be built from.
    pub fn any_sellable(&self, provider: &str, model: &str, now: i64) -> bool {
        self.accounts
            .iter()
            .any(|a| a.provider == provider && a.sell_enabled && a.lane_available(model, now))
    }

    /// Task finished OK: release the slot, decay the quota estimate, and clear
    /// the lane's failure history so the next isolated hiccup starts at the
    /// bottom of the ladder rather than near the breaker.
    pub fn on_success(&mut self, provider: &str, account_id: &str, model: &str, tokens_used: u64) {
        if let Some(a) = self.find(provider, account_id) {
            a.in_use = a.in_use.saturating_sub(1);
            a.quota_remaining = a.quota_remaining.saturating_sub(tokens_used);
            a.cooldown_until = None; // a success clears any stale cooldown
            if let Some(lane) = a.lanes.get_mut(model) {
                lane.fail_streak = 0;
                lane.cooldown_until = None;
                lane.last_error.clear();
                // A success does *not* clear a pause that needs the operator:
                // the request that succeeded came from the local direct route
                // or from another lane, and silently un-pausing here would undo
                // a deliberate decision.
                if lane.paused.is_some_and(|r| !r.requires_user()) {
                    lane.paused = None;
                    lane.resume_at = 0;
                }
            }
        }
    }

    /// Task failed: release the slot and apply the recovery ladder (§4.5).
    ///
    /// Returns the pause the lane ended up in, if any — the caller uses it to
    /// persist the state and to re-declare supply, since a pause that never
    /// reaches the gateway keeps attracting requests this device will fail.
    pub fn on_error(
        &mut self,
        provider: &str,
        account_id: &str,
        model: &str,
        kind: UpstreamErrorKind,
        detail: &str,
        now: i64,
    ) -> Option<PauseReason> {
        let a = self.find(provider, account_id)?;
        a.in_use = a.in_use.saturating_sub(1);
        match kind {
            UpstreamErrorKind::RateLimited { reset_at } => {
                // A subscription's rate limit is account-wide — the window it
                // exhausted is shared by every model — so this one *does* cool
                // the account as well as the lane it surfaced on.
                let until = reset_at
                    .filter(|r| *r > now)
                    .unwrap_or(now + COOLDOWN_RATE_LIMIT_SECS);
                a.cooldown_until = Some(until.max(a.cooldown_until.unwrap_or(0)));
                let lane = a.lanes.entry(model.to_string()).or_default();
                lane.last_error = detail.to_string();
                // Not a fault: the upstream told us when to come back, so this
                // resumes itself and never reaches the breaker.
                lane.paused = Some(PauseReason::RateLimit);
                lane.resume_at = until;
                lane.cooldown_until = Some(until);
                Some(PauseReason::RateLimit)
            }
            UpstreamErrorKind::ServerError => {
                // Lane-scoped on purpose: a 5xx on one model says nothing about
                // the account's other models, and cooling the whole account
                // here used to take healthy capacity off the market with it.
                // The ladder below is what backs this lane off.
                let lane = a.lanes.entry(model.to_string()).or_default();
                lane.last_error = detail.to_string();
                lane.fail_streak += 1;
                match LANE_COOLDOWN_LADDER.get(lane.fail_streak as usize - 1) {
                    // Early failures are almost always transient; back off and
                    // retry without bothering anyone.
                    Some(secs) => {
                        lane.cooldown_until = Some(now + secs);
                        None
                    }
                    // Out of rungs: this lane is not having a bad minute, it is
                    // broken. Stop selling it and let the operator look.
                    None => {
                        lane.paused = Some(PauseReason::Breaker);
                        lane.resume_at = 0;
                        lane.cooldown_until = None;
                        Some(PauseReason::Breaker)
                    }
                }
            }
            UpstreamErrorKind::AuthFailed => {
                a.auth_failed = true;
                // Credentials are the account's, so every lane of it is out.
                for lane in a.lanes.values_mut() {
                    lane.paused = Some(PauseReason::Auth);
                    lane.resume_at = 0;
                }
                if let Some(lane) = a.lanes.get_mut(model) {
                    lane.last_error = detail.to_string();
                }
                Some(PauseReason::Auth)
            }
        }
    }

    /// Put a lane out of service explicitly (operator switch, spent quota, a
    /// gateway `lane.pause` control frame).
    pub fn pause_lane(&mut self, provider: &str, account_id: &str, model: &str, reason: PauseReason, resume_at: i64) {
        if let Some(a) = self.find(provider, account_id) {
            let lane = a.lanes.entry(model.to_string()).or_default();
            lane.paused = Some(reason);
            lane.resume_at = resume_at;
        }
    }

    /// Clear every exclusion on a lane — the operator says it is fixed.
    /// Also clears the account-level auth flag when the pause was an auth
    /// failure, since that is what was blocking every other lane too.
    pub fn resume_lane(&mut self, provider: &str, account_id: &str, model: &str) {
        if let Some(a) = self.find(provider, account_id) {
            let was_auth = a.lanes.get(model).and_then(|l| l.paused) == Some(PauseReason::Auth);
            if was_auth {
                a.auth_failed = false;
            }
            a.cooldown_until = None;
            if let Some(lane) = a.lanes.get_mut(model) {
                *lane = LaneState::default();
            }
        }
    }

    /// Resume every lane of every account that is out for a reason the operator
    /// can clear. Used by the "resume selling" action when no lane is named.
    pub fn resume_all(&mut self) {
        for a in &mut self.accounts {
            a.auth_failed = false;
            a.cooldown_until = None;
            for lane in a.lanes.values_mut() {
                *lane = LaneState::default();
            }
        }
    }

    /// The earliest instant at which some lane comes back on its own, so the
    /// daemon can wake up exactly then and re-declare instead of waiting out
    /// the periodic tick.
    pub fn next_auto_resume(&self, now: i64) -> Option<i64> {
        self.accounts
            .iter()
            .flat_map(|a| a.lanes.values())
            .filter_map(|l| l.auto_resume_at(now))
            .min()
    }

    /// Every lane's current state, for the UI and for building the supply
    /// declaration.
    pub fn lane_views(&self, now: i64) -> Vec<LaneStatusView> {
        let mut out = Vec::new();
        for a in &self.accounts {
            for (model, lane) in &a.lanes {
                let (status, reason) = if let Some(r) = lane.paused {
                    ("paused", Some(r))
                } else if lane.cooldown_until.is_some_and(|c| c > now) {
                    ("cooldown", None)
                } else if !a.sell_enabled {
                    ("off", None)
                } else if a.available(now) {
                    ("selling", None)
                } else {
                    // The account itself is expired/exhausted; the lane is fine.
                    (a.status(now), None)
                };
                out.push(LaneStatusView {
                    provider: a.provider.clone(),
                    account_id: a.account_id.clone(),
                    model: model.clone(),
                    status: status.to_string(),
                    paused_reason: reason.map(|r| r.as_str().to_string()),
                    requires_user: reason.is_some_and(|r| r.requires_user()),
                    resume_at: if lane.resume_at > now { lane.resume_at } else { 0 },
                    cooldown_until: lane.cooldown_until.filter(|c| *c > now),
                    fail_streak: lane.fail_streak,
                    last_error: lane.last_error.clone(),
                    sell_enabled: a.sell_enabled,
                    quota_remaining: a.quota_remaining,
                });
            }
        }
        out
    }

    /// Snapshot for the UI (spec §2.1 `list_accounts`).
    pub fn statuses(&self, now: i64) -> Vec<AccountStatusView> {
        self.accounts
            .iter()
            .map(|a| AccountStatusView {
                provider: a.provider.clone(),
                account_id: a.account_id.clone(),
                plan: a.plan.clone(),
                quota_remaining: a.quota_remaining,
                status: a.status(now).to_string(),
                expires_at: a.expires_at,
                cooldown_until: a.cooldown_until.filter(|c| *c > now),
                sell_enabled: a.sell_enabled,
                origin: a.origin.clone(),
                used_today: a.used_today,
                sell_daily_limit: a.sell_daily_limit,
            })
            .collect()
    }

    fn find(&mut self, provider: &str, account_id: &str) -> Option<&mut AccountRuntime> {
        self.accounts
            .iter_mut()
            .find(|a| a.provider == provider && a.account_id == account_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPUS: &str = "claude-opus-5";
    const HAIKU: &str = "claude-haiku-4-5";

    fn acct(id: &str, quota: u64) -> AccountRuntime {
        let mut a = AccountRuntime::new("claude", id, &format!("claude:{id}")).with_models([OPUS, HAIKU]);
        a.quota_remaining = quota;
        a
    }

    #[test]
    fn round_robin_rotates_least_recent_first() {
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![acct("a", 100), acct("b", 100)]);
        let first = p.pick("claude", 10).unwrap();
        p.on_success("claude", &first.account_id, OPUS, 1);
        let second = p.pick("claude", 11).unwrap();
        assert_ne!(first.account_id, second.account_id, "round-robin must alternate");
    }

    #[test]
    fn fill_first_prefers_larger_quota() {
        let mut p = AccountPool::new(Strategy::FillFirst);
        p.set_accounts(vec![acct("small", 10), acct("big", 1000)]);
        assert_eq!(p.pick("claude", 1).unwrap().account_id, "big");
    }

    #[test]
    fn cooldown_and_recovery() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![acct("a", 100)]);
        let picked = p.pick_local("claude", OPUS, now).unwrap();
        p.on_error("claude", &picked.account_id, OPUS, UpstreamErrorKind::ServerError, "500", now);
        // A 5xx is scoped to the lane it happened on: local traffic for that
        // model backs off, everything else keeps working.
        assert!(p.pick_local("claude", OPUS, now + 1).is_none());
        assert!(p.pick_local("claude", HAIKU, now + 1).is_some());
        // After the ladder's first rung elapses it becomes usable again.
        assert!(p.pick_local("claude", OPUS, now + LANE_COOLDOWN_LADDER[0] + 1).is_some());
    }

    #[test]
    fn rate_limit_cools_longer_and_honors_reset() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![acct("a", 100)]);
        let picked = p.pick("claude", now).unwrap();
        p.on_error(
            "claude",
            &picked.account_id,
            OPUS,
            UpstreamErrorKind::RateLimited { reset_at: Some(now + 5000) },
            "429",
            now,
        );
        // Still cooling after the transient window — rate limits cool longer.
        assert!(p.pick("claude", now + COOLDOWN_TRANSIENT_SECS + 1).is_none());
        assert!(p.pick("claude", now + 5001).is_some());
    }

    #[test]
    fn auth_failure_marks_expired_until_refreshed() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        let mut a = acct("a", 100);
        a.expires_at = Some(now + 3600);
        p.set_accounts(vec![a.clone()]);
        let picked = p.pick("claude", now).unwrap();
        p.on_error("claude", &picked.account_id, OPUS, UpstreamErrorKind::AuthFailed, "401", now);
        assert_eq!(p.statuses(now)[0].status, "expired");
        assert!(p.pick("claude", now).is_none());

        // Rebuild with the same expiry keeps the failure sticky…
        p.set_accounts(vec![a.clone()]);
        assert_eq!(p.statuses(now)[0].status, "expired");
        // …but a refreshed token (new expiry) clears it.
        a.expires_at = Some(now + 7200);
        p.set_accounts(vec![a]);
        assert_eq!(p.statuses(now)[0].status, "available");
    }

    #[test]
    fn expired_and_exhausted_are_skipped_but_others_serve() {
        let now = 1000;
        let mut expired = acct("expired", 100);
        expired.expires_at = Some(now - 1);
        let exhausted = acct("empty", 0);
        let ok = acct("ok", 50);
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![expired, exhausted, ok]);
        assert_eq!(p.pick("claude", now).unwrap().account_id, "ok");
        assert!(p.any_available("claude", now));
        // Unknown provider → nothing.
        assert!(p.pick("gemini", now).is_none());
    }

    #[test]
    fn selling_is_per_account_not_per_provider() {
        let now = 1000;
        let mut off = acct("shared-with-local-cli", 100);
        let mut on = acct("asale-owned", 100);
        on.sell_enabled = true;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        off.sell_enabled = false;
        p.set_accounts(vec![off, on]);

        // The relay only ever serves from the account switched on for selling…
        assert_eq!(p.pick_for_sale("claude", OPUS, now).unwrap().account_id, "asale-owned");
        assert!(p.any_sellable("claude", OPUS, now));
        // …while the local direct route may still use either.
        assert!(p.pick("claude", now).is_some());

        // Switching the only sellable account off stops sales but not local use.
        let mut both_off = acct("asale-owned", 100);
        both_off.sell_enabled = false;
        p.set_accounts(vec![both_off]);
        assert!(p.pick_for_sale("claude", OPUS, now).is_none());
        assert!(!p.any_sellable("claude", OPUS, now));
        assert!(p.any_available("claude", now));
    }

    #[test]
    fn concurrency_slots_are_leased_and_released() {
        let now = 1000;
        let mut a = acct("a", 100);
        a.concurrency_max = 1;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![a]);
        let picked = p.pick("claude", now).unwrap();
        assert!(p.pick("claude", now).is_none(), "single slot is leased");
        p.on_success("claude", &picked.account_id, OPUS, 10);
        assert!(p.pick("claude", now).is_some(), "released after completion");
        // Quota decayed by the reported usage.
        assert_eq!(p.statuses(now)[0].quota_remaining, 90);
    }

    // ── lanes ───────────────────────────────────────────────────────

    fn sellable(id: &str) -> AccountRuntime {
        let mut a = acct(id, 1_000_000);
        a.sell_enabled = true;
        a
    }

    /// One failing model must not take the account's other models down with it.
    #[test]
    fn a_failing_lane_does_not_stop_the_accounts_other_models() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pick_for_sale("claude", OPUS, now).unwrap();
        p.on_error("claude", "a", OPUS, UpstreamErrorKind::ServerError, "500", now);

        assert!(p.pick_for_sale("claude", OPUS, now).is_none(), "the failed lane is cooling");
        assert!(p.pick_for_sale("claude", HAIKU, now).is_some(), "its sibling keeps selling");
        assert!(!p.any_sellable("claude", OPUS, now));
        assert!(p.any_sellable("claude", HAIKU, now));
    }

    #[test]
    fn transient_failures_back_off_then_trip_the_breaker() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);

        // First two failures self-heal on the ladder.
        let mut at = now;
        for (i, rung) in LANE_COOLDOWN_LADDER.iter().enumerate() {
            p.pick_for_sale("claude", OPUS, at).unwrap();
            assert_eq!(p.on_error("claude", "a", OPUS, UpstreamErrorKind::ServerError, "500", at), None);
            assert!(p.pick_for_sale("claude", OPUS, at + rung - 1).is_none(), "rung {i} still cooling");
            at += rung + 1;
            assert!(p.pick_for_sale("claude", OPUS, at).is_some(), "rung {i} must clear on its own");
            // That pick leased a slot; hand it back without clearing the streak
            // (only a success does that).
            p.find("claude", "a").unwrap().in_use -= 1;
        }

        // The third consecutive failure is not a bad minute — it is a broken
        // lane, and it stays out until someone looks at it.
        p.pick_for_sale("claude", OPUS, at).unwrap();
        assert_eq!(
            p.on_error("claude", "a", OPUS, UpstreamErrorKind::ServerError, "500", at),
            Some(PauseReason::Breaker)
        );
        assert_eq!(p.find("claude", "a").unwrap().lanes[OPUS].fail_streak, LANE_BREAKER_THRESHOLD);
        assert!(p.pick_for_sale("claude", OPUS, at + 86_400).is_none(), "a breaker does not time out");

        let view = p.lane_views(at).into_iter().find(|v| v.model == OPUS).unwrap();
        assert_eq!(view.status, "paused");
        assert_eq!(view.paused_reason.as_deref(), Some("breaker"));
        assert!(view.requires_user, "the UI must offer a resume button for this");

        // …and the operator's resume puts it straight back.
        p.resume_lane("claude", "a", OPUS);
        assert!(p.pick_for_sale("claude", OPUS, at).is_some());
        assert_eq!(p.find("claude", "a").unwrap().lanes[OPUS].fail_streak, 0);
    }

    #[test]
    fn a_success_resets_the_ladder() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pick_for_sale("claude", OPUS, now).unwrap();
        p.on_error("claude", "a", OPUS, UpstreamErrorKind::ServerError, "500", now);
        let later = now + LANE_COOLDOWN_LADDER[0] + 1;
        p.pick_for_sale("claude", OPUS, later).unwrap();
        p.on_success("claude", "a", OPUS, 100);
        assert_eq!(p.find("claude", "a").unwrap().lanes[OPUS].fail_streak, 0);
        // So the next isolated failure starts at the first rung again, rather
        // than landing one step from the breaker.
        p.pick_for_sale("claude", OPUS, later + 1).unwrap();
        assert_eq!(
            p.on_error("claude", "a", OPUS, UpstreamErrorKind::ServerError, "500", later + 1),
            None
        );
    }

    #[test]
    fn a_rate_limit_resumes_itself_at_the_reset() {
        let now = 1000;
        let reset = now + 600;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pick_for_sale("claude", OPUS, now).unwrap();
        let paused = p.on_error(
            "claude",
            "a",
            OPUS,
            UpstreamErrorKind::RateLimited { reset_at: Some(reset) },
            "429",
            now,
        );
        assert_eq!(paused, Some(PauseReason::RateLimit));
        assert!(!PauseReason::RateLimit.requires_user(), "nobody should have to click through a 429");

        let view = p.lane_views(now).into_iter().find(|v| v.model == OPUS).unwrap();
        assert_eq!(view.resume_at, reset);
        assert!(!view.requires_user);
        // The daemon schedules its wake-up from this.
        assert_eq!(p.next_auto_resume(now), Some(reset));
    }

    #[test]
    fn an_auth_failure_takes_every_lane_and_a_refresh_gives_them_back() {
        let now = 1000;
        let mut a = sellable("a");
        a.expires_at = Some(now + 3600);
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![a.clone()]);
        p.pick_for_sale("claude", OPUS, now).unwrap();
        assert_eq!(
            p.on_error("claude", "a", OPUS, UpstreamErrorKind::AuthFailed, "401", now),
            Some(PauseReason::Auth)
        );
        // Credentials are the account's, so both lanes go — unlike a 5xx.
        for v in p.lane_views(now) {
            assert_eq!(v.paused_reason.as_deref(), Some("auth"));
            assert!(v.requires_user);
        }
        assert!(p.next_auto_resume(now).is_none(), "auth never clears on a timer");

        // A refreshed token (new expiry) is the recovery signal.
        a.expires_at = Some(now + 7200);
        p.set_accounts(vec![a]);
        assert!(p.pick_for_sale("claude", OPUS, now).is_some());
        assert!(p.lane_views(now).iter().all(|v| v.paused_reason.is_none()));
    }

    #[test]
    fn a_rebuild_does_not_forget_lane_state() {
        // rebuild_pool runs every minute; if it wiped lane state a broken lane
        // would quietly go back on the market a minute after tripping.
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pause_lane("claude", "a", OPUS, PauseReason::Breaker, 0);
        p.set_accounts(vec![sellable("a")]);
        assert!(p.pick_for_sale("claude", OPUS, now).is_none());
        assert!(p.pick_for_sale("claude", HAIKU, now).is_some());
    }

    #[test]
    fn local_use_is_unaffected_by_lane_pauses() {
        // Pauses are about selling. Your own subscription, used locally, is not
        // a sale — a breaker on the market side must not lock you out of it.
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pause_lane("claude", "a", OPUS, PauseReason::Breaker, 0);
        assert!(p.pick_for_sale("claude", OPUS, now).is_none());
        assert!(p.pick("claude", now).is_some());
    }

    #[test]
    fn next_auto_resume_picks_the_earliest_and_ignores_manual_ones() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a"), sellable("b")]);
        p.pause_lane("claude", "a", OPUS, PauseReason::RateLimit, now + 900);
        p.pause_lane("claude", "b", OPUS, PauseReason::Quota, now + 300);
        p.pause_lane("claude", "b", HAIKU, PauseReason::Breaker, 0);
        assert_eq!(p.next_auto_resume(now), Some(now + 300));
        // Once everything left is operator-gated there is nothing to wake for.
        p.pause_lane("claude", "a", OPUS, PauseReason::Auth, 0);
        p.pause_lane("claude", "b", OPUS, PauseReason::Auth, 0);
        assert_eq!(p.next_auto_resume(now), None);
    }
}
