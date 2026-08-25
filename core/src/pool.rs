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
//!
//! # The price band (§4.6)
//!
//! A lane can also be held back for a reason that is not a fault at all: the
//! market price has moved outside the band its account is willing to sell in.
//! That is a *price* decision, so it is kept apart from the pause ladder above
//! — it needs no operator, has no resume instant, and must not consume a rung
//! of the breaker. See [`LaneState::price_withheld`] and [`apply_price_band`].
//!
//! Prices here are always **percent of the vendor's list price** (`10..=100`,
//! the range the server clamps `mkt_ratio` to), never percent off it. That is
//! the number the seller decides about — "I will not sell below 60% of list" —
//! and keeping one convention end to end is what stops a band from silently
//! meaning its own inverse.

use asale_protocol::ids::Wire;
use serde::Serialize;
use std::collections::BTreeMap;

/// Default cooldown after a transient upstream failure (5xx), seconds.
pub const COOLDOWN_TRANSIENT_SECS: i64 = 300;
/// Cooldown after a rate-limit (429) when the upstream gives no reset, seconds.
/// Rate limits are window-scoped, so cool longer than plain transient errors.
pub const COOLDOWN_RATE_LIMIT_SECS: i64 = 900;
/// How long a lane stays off the market after its upstream said the model does
/// not exist, seconds.
///
/// A day, because the answer is a fact about the vendor's catalogue rather than
/// about this minute: retrying sooner spends a buyer's request to re-learn
/// something that changes when a vendor ships a release, and never retrying
/// means a model that comes back — or an entitlement that is granted — stays
/// unsold until the daemon restarts.
pub const COOLDOWN_UNSUPPORTED_SECS: i64 = 24 * 3600;
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
    /// 401 (or a 403 the upstream framed as a credential problem) — the
    /// operator has to sign in again.
    Auth,
    /// The upstream refuses this machine's requests for a reason that is not
    /// about the credential: a region block, a network middlebox, a CDN rule.
    ///
    /// Kept apart from [`PauseReason::Auth`] because the two need opposite
    /// actions from the operator. Anthropic answers a request from a region it
    /// does not serve with `403 {"type":"forbidden","message":"Request not
    /// allowed"}` — the login is fine and signing in again changes nothing, so
    /// telling somebody to re-authenticate sends them to fix the one thing that
    /// is not broken while their subscription sits off the market.
    Blocked,
    /// Too many consecutive failures; the operator has to look at it.
    Breaker,
    /// The operator switched this lane off.
    Manual,
    /// The platform does not trade this model (no price row, or an operator
    /// disabled it). Nothing the seller can fix locally, and nothing to wait
    /// out either — it clears when the catalog lists the model again.
    Untradable,
    /// The *upstream* does not serve this model to this account: the vendor
    /// answered "no such model" for an id this device was advertising.
    ///
    /// The mirror image of [`PauseReason::Untradable`] — that one is the
    /// platform declining to trade a model the subscription can serve, this one
    /// is the subscription unable to serve a model the platform trades. Kept
    /// apart from the breaker because it is not a run of bad luck to back off
    /// from: every request for this id will fail the same way, so the lane has
    /// to come off the market on the *first* one rather than after three, and
    /// nothing an operator can do would change it.
    ///
    /// It still carries a resume instant ([`COOLDOWN_UNSUPPORTED_SECS`]) rather
    /// than waiting for a person: a model can come back, an entitlement can be
    /// granted, and the catalog can be wrong about which id a vendor answers to.
    /// One retry a day costs one failed request and is the only thing that would
    /// ever find out.
    Unsupported,
}

impl PauseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            PauseReason::RateLimit => "rate_limit",
            PauseReason::Quota => "quota",
            PauseReason::Auth => "auth",
            PauseReason::Blocked => "blocked",
            PauseReason::Breaker => "breaker",
            PauseReason::Manual => "manual",
            PauseReason::Untradable => "untradable",
            PauseReason::Unsupported => "unsupported",
        }
    }

    pub fn parse(s: &str) -> Option<PauseReason> {
        Some(match s {
            "rate_limit" => PauseReason::RateLimit,
            "quota" => PauseReason::Quota,
            "auth" => PauseReason::Auth,
            "blocked" => PauseReason::Blocked,
            "breaker" => PauseReason::Breaker,
            "manual" => PauseReason::Manual,
            "untradable" => PauseReason::Untradable,
            "unsupported" => PauseReason::Unsupported,
            _ => return None,
        })
    }

    /// Whether clearing this needs a person. The UI shows a "resume selling"
    /// button exactly for these; the rest disappear on their own, and offering
    /// a button for them would only invite the operator to fight a countdown.
    pub fn requires_user(self) -> bool {
        matches!(
            self,
            PauseReason::Auth | PauseReason::Blocked | PauseReason::Breaker | PauseReason::Manual
        )
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
    /// What the market currently pays for this model, in whole percent of the
    /// vendor's list price. `None` until the device has seen a price for it —
    /// which is not the same as 0, and must never be read as "free".
    pub ratio: Option<i32>,
    /// Held back because the price above sits outside the account's band.
    /// Not a `paused` reason: nothing is wrong with this lane, so it takes no
    /// rung of the breaker ladder and needs no operator to come back.
    pub price_withheld: bool,
    /// The band the flag above was decided under. Editing the band is the
    /// operator changing their mind, not the market moving, so it clears the
    /// hysteresis: someone who has just widened their floor expects the lane
    /// back now, not at the next price tick.
    pub price_band: Option<(i64, i64)>,
}

impl LaneState {
    /// The pause actually in force at `now`.
    ///
    /// A pause with a `resume_at` that has passed is over. Reading `paused`
    /// directly instead left every self-clearing pause permanent: the only
    /// thing that ever cleared one was a *successful* call on the same lane
    /// (`on_success`), which a paused lane is not sent, so on a device that
    /// only sells, one 429 took a lane off the market until the daemon was
    /// restarted. `auto_resume_at` below — and the wake-up the daemon schedules
    /// from it — were already written as if this were true.
    ///
    /// The field is left set rather than cleared: this is a `&self` read, and
    /// the stale value is harmless once every reader goes through here.
    pub fn pause_at(&self, now: i64) -> Option<PauseReason> {
        match self.paused {
            // `resume_at == 0` is "no instant to come back at" — the pauses
            // that wait for an operator, and `Untradable`, which waits for the
            // catalog.
            Some(r) if r.requires_user() || self.resume_at == 0 || self.resume_at > now => Some(r),
            _ => None,
        }
    }

    /// Whether this lane may serve market traffic at `now`.
    pub fn servable(&self, now: i64) -> bool {
        self.pause_at(now).is_none()
            && !self.cooldown_until.is_some_and(|c| c > now)
            && !self.price_withheld
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

/// Percentage points the price has to move back *past* the band's edge before a
/// withheld lane returns.
///
/// Without it a price parked exactly on the edge would withdraw and re-enter on
/// alternate ticks, and every one of those flips is a supply re-declaration the
/// gateway has to reconcile.
///
/// Never more than half the band, so it cannot swallow a narrow one — and for a
/// band of zero width (`min == max`, the only terms a seller who must not trade
/// below cost can set) it is zero, which is what makes "the price is exactly my
/// floor" mean *sell*.
pub const PRICE_BAND_MARGIN_PCT: i32 = 2;

/// Apply an account's price band to one lane, with hysteresis.
///
/// `ratio` is what the market currently pays for the model, in whole percent of
/// list price (`None` = not known yet), and `band` is the account's `(min, max)`
/// on that same scale.
///
/// Withdrawal is immediate; re-entry needs the price back *past* the edge it
/// left by, and nothing else. Re-entry used to also wait out three to six
/// minutes, to keep a fleet of devices from all returning at once and collapsing
/// the ratio they had just recovered from. That protection cost more than it
/// saved: the server reprices every minute and a peak lasts one or two, so the
/// wait outlived the window that would have justified it — and any dip during
/// the wait restarted it. A lane whose floor sat at the ceiling (100%, which is
/// what a metered endpoint reselling at cost must charge) could therefore leave
/// the market once and never come back. The sellers the band exists to protect
/// were the ones it locked out.
///
/// An unknown price never withholds: a device that has not managed to read the
/// market yet should keep selling on the terms it already had, not take itself
/// off the market because it is offline.
pub fn apply_price_band(lane: &mut LaneState, ratio: Option<i32>, band: (i64, i64)) {
    lane.ratio = ratio;
    if lane.price_band != Some(band) {
        lane.price_band = Some(band);
        lane.price_withheld = false;
    }
    let (lo, hi) = (band.0 as i32, band.1 as i32);
    let Some(d) = ratio else {
        // No price to judge: release anything held on the last one rather than
        // leaving the lane stuck off for as long as the market is unreachable.
        lane.price_withheld = false;
        return;
    };
    if lane.price_withheld {
        let margin = PRICE_BAND_MARGIN_PCT.min((hi - lo) / 2);
        if d >= lo + margin && d <= hi - margin {
            lane.price_withheld = false;
        }
        return;
    }
    // Leaving the band takes effect at once — the whole point of the band is
    // not to sell at a price the operator rejected.
    if d < lo || d > hi {
        lane.price_withheld = true;
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
    /// The upstream refused this machine rather than this credential — a region
    /// block, a middlebox, a CDN rule. Backed off on the same ladder as a 5xx,
    /// because the cheap case is a proxy hiccup, and parked on
    /// [`PauseReason::Blocked`] rather than the breaker when it persists, so the
    /// operator is told what to actually go and fix.
    Blocked,
    /// The upstream does not know this model id (`404 not_found_error`, or the
    /// Codex backend's `400 … is not supported when using Codex`). Not a
    /// failure of the lane so much as proof it should never have been offered:
    /// see [`PauseReason::Unsupported`].
    Unsupported,
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
    /// The id the vendor knows this subscription by, when its upstream requires
    /// that id to travel with the bearer (Codex's `chatgpt-account-id`). Not a
    /// secret, and not the same thing as `account_id` — that one is asale's own
    /// key for the account, usually an email.
    pub upstream_account_id: Option<String>,
    /// Where this account's requests actually go, for the one provider whose
    /// host is not known at compile time (`custom`). `None` everywhere else:
    /// every other provider's upstream is the vendor's, and the gateway builds
    /// the URL for it.
    pub upstream_base: Option<String>,
    /// The dialect that endpoint speaks, for the same one provider. It decides
    /// the path under `upstream_base` and the header the key is sent in, so it
    /// travels with the base everywhere the base does. `None` reads as the
    /// OpenAI schema — what every custom account spoke before this was a
    /// choice, and still what most of them do.
    pub upstream_wire: Option<Wire>,
    /// Whether that same endpoint also serves the Responses route, as probed
    /// when it was connected. `false` everywhere else: every other provider's
    /// upstream has one route by definition, and a custom endpoint nobody has
    /// asked keeps the route it already works on.
    ///
    /// What it buys is [`AccountRuntime::wire_for`] — a model that cannot be
    /// served on chat/completions at all is offered on the route that can.
    pub upstream_responses: bool,
    /// market model id -> the id this account's upstream knows it by.
    ///
    /// Only a `custom` account has any: an aggregator lists
    /// `anthropic/claude-haiku-4.5` for what the market trades as
    /// `claude-haiku-4-5`, so the lane is declared under the market id and the
    /// endpoint's own spelling is put back when the request is sent. Empty
    /// everywhere else, where the two ids are the same string.
    pub model_aliases: BTreeMap<String, String>,
    pub plan: Option<String>,
    /// Estimated serviceable tokens remaining in the account's window.
    pub quota_remaining: u64,
    /// When the spent window comes back, when the *provider* said so.
    ///
    /// Only set while `quota_remaining` is 0 and the number came from the
    /// upstream's own rate-limit windows (see [`crate::quota`]). The local
    /// estimate has no such instant to offer — its window is a rolling sum that
    /// recovers a token at a time rather than resetting — so it leaves this
    /// `None` and the account waits for the next minute's rebuild to notice.
    pub quota_reset_at: Option<i64>,
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
    /// The models this account is switched on to sell. Empty means all of them,
    /// which is the default and what every account that predates the setting
    /// keeps meaning — see `store::ToolRow::sell_models`.
    pub sell_models: Vec<String>,
    /// The price band this account sells inside, in whole percent *of* list
    /// price. `(5, 100)` is the whole legal range and never withholds anything;
    /// a fresh account starts at the default floor of 10 instead.
    pub sell_min_ratio: i64,
    pub sell_max_ratio: i64,
    pub cooldown_until: Option<i64>,
    /// Set on 401/403; cleared when a rebuild sees a newer expiry (refreshed).
    pub auth_failed: bool,
    pub last_used: i64,
    pub in_use: u32,
    /// How many requests this account serves at once — the local lease ceiling,
    /// and the number declared to the market as the lane's concurrency total.
    /// Set from the account's `sell_concurrency` on every pool rebuild.
    pub concurrency_max: u32,
    /// Per-model serving state, keyed by model id. The key set is this
    /// account's model catalogue: a model with no entry here is not sold.
    pub lanes: BTreeMap<String, LaneState>,
}

/// Models whose upstream cannot serve a tool-carrying request on the chat
/// route at all.
///
/// OpenAI's "responses lite" generation — `gpt-5.6-sol`, `-luna`, `-terra` —
/// answers `/v1/chat/completions` with a `400` the moment `tools` and
/// `reasoning_effort` arrive together:
///
/// ```text
/// Function tools with reasoning_effort are not supported for gpt-5.6-luna in
/// /v1/chat/completions. To use function tools, use /v1/responses or set
/// reasoning_effort to 'none'.
/// ```
///
/// Which is every agent request there is — the buyer sends a harness. Dropping
/// the buyer's reasoning to satisfy the chat route would sell them a model that
/// does not think; the route that takes both is `/responses`, and a custom
/// endpoint proxying OpenAI almost always serves it.
///
/// Named one by one rather than matched on `gpt-5.6-`, because that generation
/// is not uniform: `gpt-5.6-codex` is a full Responses model and serves the
/// chat route like any other, so a prefix would drop it from every chat-only
/// endpoint that can in fact sell it.
///
/// Codex's own catalog carries the authoritative answer as `use_responses_lite`
/// (see `codex_catalog`), but only for an account holding a Codex credential —
/// a custom endpoint is never told. So this list is what a custom endpoint has,
/// and a later lite generation needs a line here.
//
// ponytail: the upgrade path is that same flag on `SellableCatalog`, which
// would make this list a fallback rather than the source.
pub fn needs_responses_wire(model: &str) -> bool {
    const LITE: &[&str] = &["gpt-5.6-sol", "gpt-5.6-luna", "gpt-5.6-terra"];
    LITE.contains(&model)
}

impl AccountRuntime {
    pub fn new(provider: &str, account_id: &str, keychain_ref: &str) -> AccountRuntime {
        AccountRuntime {
            provider: provider.to_string(),
            account_id: account_id.to_string(),
            keychain_ref: keychain_ref.to_string(),
            upstream_account_id: None,
            upstream_base: None,
            upstream_wire: None,
            upstream_responses: false,
            model_aliases: BTreeMap::new(),
            plan: None,
            quota_remaining: 0,
            quota_reset_at: None,
            expires_at: None,
            sell_enabled: false,
            origin: None,
            used_today: 0,
            sell_daily_limit: 0,
            sell_models: Vec::new(),
            sell_min_ratio: crate::store::DEFAULT_SELL_MIN_RATIO,
            sell_max_ratio: crate::store::RATIO_BAND_FULL.1,
            cooldown_until: None,
            auth_failed: false,
            last_used: 0,
            in_use: 0,
            concurrency_max: crate::store::DEFAULT_SELL_CONCURRENCY as u32,
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

    /// The dialect this account speaks *for one model*.
    ///
    /// Normally its own — one endpoint, one protocol. The exception is
    /// [`needs_responses_wire`]: those models refuse the chat route outright
    /// once a request carries tools, and an endpoint that proxies OpenAI
    /// usually serves both routes, so they are offered on the one that works.
    /// Every other model on the same account keeps chat/completions, which is
    /// where their aliases and their tool schema already work.
    pub fn wire_for(&self, model: &str) -> Option<Wire> {
        match self.upstream_wire {
            Some(Wire::Openai) if self.upstream_responses && needs_responses_wire(model) => Some(Wire::Responses),
            w => w,
        }
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

    /// Whether the operator has this model switched on for selling.
    ///
    /// Separate from `sell_enabled`, which is the whole account's switch: this
    /// one is the per-model narrowing, and an empty selection means every model
    /// rather than none (an account nobody has narrowed sells what it always
    /// did).
    pub fn sells_model(&self, model: &str) -> bool {
        self.sell_models.is_empty() || self.sell_models.iter().any(|m| m == model)
    }

    /// Whether this account may *sell* one model right now: the model has to be
    /// one the operator offers, the account has to be healthy, and the lane
    /// neither cooling nor paused.
    fn lane_available(&self, model: &str, now: i64) -> bool {
        self.sells_model(model) && self.available(now) && self.lanes.get(model).is_some_and(|l| l.servable(now))
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
    /// See [`AccountRuntime::upstream_account_id`]. Carried on the pick because
    /// the caller that injects the bearer is the same one that has to send it.
    pub upstream_account_id: Option<String>,
    /// See [`AccountRuntime::upstream_base`]. Carried for the same reason: the
    /// executor that sends the request is the only place that knows which
    /// account — and therefore which endpoint — the task was leased against.
    pub upstream_base: Option<String>,
    /// See [`AccountRuntime::upstream_wire`]. Carried alongside the base, which
    /// is meaningless without it.
    pub upstream_wire: Option<Wire>,
    /// The id this account's upstream knows the leased model by, when it differs
    /// from the market's. `None` means "send the model id as it arrived".
    pub upstream_model: Option<String>,
}

/// Serializable status view for `list_accounts`.
#[derive(Debug, Clone, Serialize)]
pub struct AccountStatusView {
    pub provider: String,
    pub account_id: String,
    pub plan: Option<String>,
    pub quota_remaining: u64,
    /// When an exhausted account's window resets, when the provider named an
    /// instant. `None` on a healthy account and on one whose headroom is only
    /// a local estimate (see `AccountRuntime::quota_reset_at`).
    pub quota_reset_at: Option<i64>,
    pub status: String,
    pub expires_at: Option<i64>,
    pub cooldown_until: Option<i64>,
    pub sell_enabled: bool,
    /// The models this account sells; empty = all of them. What the sell page
    /// seeds its model picker with.
    pub sell_models: Vec<String>,
    pub origin: Option<String>,
    pub used_today: u64,
    pub sell_daily_limit: i64,
    pub sell_min_ratio: i64,
    pub sell_max_ratio: i64,
    /// Requests this account serves at once (see `AccountRuntime::concurrency_max`).
    pub sell_concurrency: i64,
}

/// One `(account, model)` lane, as the UI and the declaration builder see it.
#[derive(Debug, Clone, Serialize)]
pub struct LaneStatusView {
    pub provider: String,
    pub account_id: String,
    pub model: String,
    /// selling | cooldown | paused | withheld | off | expired | exhausted
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
    /// What the market pays for this model, in whole percent of list price;
    /// `None` when this device has not read a price for it yet. This is what
    /// the sell page ranks its models by.
    pub ratio: Option<i32>,
    /// The band this lane's account is willing to sell inside — carried on the
    /// lane so the chart can draw the threshold next to the bar it explains.
    pub min_ratio: i64,
    pub max_ratio: i64,
    /// The account's concurrency ceiling, carried on the lane because that is
    /// the granularity the declaration is built at: one market lane is every
    /// account of a provider that sells this model, and what it may run at once
    /// is those accounts' ceilings added up.
    pub concurrency_max: u32,
    /// The dialect this lane's account speaks, for the one provider whose
    /// accounts may each speak a different one (`custom`). `None` everywhere
    /// else — and it is what marks a view as belonging to such an account.
    pub upstream_wire: Option<Wire>,
    /// Where this account's requests go, for that same provider. Carried on
    /// the lane so the declaration can mark *which upstream* it is offering:
    /// repointing a custom endpoint changes what a buyer is served by while
    /// leaving the account id, and therefore the lane, looking identical.
    pub upstream_base: Option<String>,
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

    /// Re-judge every lane against its account's price band.
    ///
    /// `ratios` maps a model to what the market pays for it, in whole percent of
    /// list price; a model missing from it has no known price, and
    /// [`apply_price_band`] leaves such a lane selling.
    ///
    /// Called right after `set_accounts` (which restores the hysteresis state
    /// this reads) on every pool rebuild, so a lane's verdict is never older
    /// than the last price the device managed to read.
    pub fn apply_prices(&mut self, ratios: &BTreeMap<String, i32>) {
        for a in &mut self.accounts {
            let band = (a.sell_min_ratio, a.sell_max_ratio);
            for (model, lane) in &mut a.lanes {
                apply_price_band(lane, ratios.get(model).copied(), band);
            }
        }
    }

    /// Pick an available account for `provider` and lease one concurrency slot.
    /// Used by the local consumer proxy's `direct` route — running your own
    /// subscription locally is not a sale, so neither the sell switch nor the
    /// lane pauses (which are about selling) apply.
    pub fn pick(&mut self, provider: &str, now: i64) -> Option<PickedAccount> {
        self.pick_where(provider, None, None, now, false)
    }

    /// Pick an account for local (non-sale) use of one model. Honours the
    /// lane's cooldown — a lane that just failed is unhealthy for local traffic
    /// too — but not its market pauses.
    pub fn pick_local(&mut self, provider: &str, model: &str, now: i64) -> Option<PickedAccount> {
        self.pick_where(provider, Some(model), None, now, false)
    }

    /// Pick an account that is switched on for selling *and* whose lane for
    /// this model is serving — the only accounts the relay executor may serve
    /// market traffic from.
    ///
    /// `wire` narrows it to accounts speaking one dialect. The lane was
    /// declared under a single one (see [`AccountPool::lane_wire`]) and the
    /// gateway has already built a body in it, so an account speaking another
    /// is not a substitute — it would answer 400 to a request it never
    /// understood. `None` does not narrow, which is every provider whose
    /// accounts all speak the same dialect by construction.
    pub fn pick_for_sale(
        &mut self,
        provider: &str,
        model: &str,
        wire: Option<Wire>,
        now: i64,
    ) -> Option<PickedAccount> {
        self.pick_where(provider, Some(model), wire, now, true)
    }

    /// Whether a failed [`AccountPool::pick_for_sale`] was *only* this account's
    /// own concurrency ceiling.
    ///
    /// The one refusal that says nothing about the lane. Every other reason
    /// `pick_for_sale` comes back empty — no such account, sell switched off,
    /// credential expired, quota spent, lane cooling or paused — is a condition
    /// that lasts, and reporting it is the honest answer. A full lease table is
    /// not: the account is healthy, this model is on offer, and a slot frees up
    /// as soon as one of the calls in flight finishes.
    ///
    /// It has to be answerable separately because the market can and does
    /// over-dispatch. An account's ceiling is one budget shared by every model
    /// it sells, but it is declared to the gateway once *per lane*
    /// (`publisher::declare`), so an account selling five models tells the market
    /// it can take five at once five times over. The gateway is within its rights
    /// to use all of it; this side is the only one that knows the accounting is
    /// shared, so this side has to absorb the difference instead of handing the
    /// work back — see `executor::execute`, and `TaskOutcome` for what handing it
    /// back costs the lane.
    pub fn lane_saturated(&self, provider: &str, model: &str, wire: Option<Wire>, now: i64) -> bool {
        self.accounts.iter().any(|a| {
            a.provider == provider
                && a.sell_enabled
                && a.sells_model(model)
                && !(wire.is_some() && a.wire_for(model).is_some() && a.wire_for(model) != wire)
                // Everything `lane_available` asks for except the free slot.
                && a.status(now) == "available"
                && a.in_use >= a.concurrency_max
                && a.lanes.get(model).is_some_and(|l| l.servable(now))
        })
    }

    fn pick_where(
        &mut self,
        provider: &str,
        model: Option<&str>,
        wire: Option<Wire>,
        now: i64,
        sell_only: bool,
    ) -> Option<PickedAccount> {
        let mut best: Option<usize> = None;
        for (i, a) in self.accounts.iter().enumerate() {
            if a.provider != provider || (sell_only && !a.sell_enabled) {
                continue;
            }
            // Only ever narrows accounts that have a dialect of their own; a
            // provider whose upstream is the vendor's is not excluded by it.
            // Asked per model, because one endpoint can speak two: see
            // [`AccountRuntime::wire_for`].
            let own_wire = model.map_or(a.upstream_wire, |m| a.wire_for(m));
            if wire.is_some() && own_wire.is_some() && own_wire != wire {
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
            upstream_account_id: a.upstream_account_id.clone(),
            upstream_base: a.upstream_base.clone(),
            upstream_wire: model.map_or(a.upstream_wire, |m| a.wire_for(m)),
            // Only when this account spells the model differently from the
            // market. `pick` (the local, non-sale route) passes no model, and
            // there is nothing to translate for it either — a local caller is
            // talking to its own vendor with its own ids.
            upstream_model: model.and_then(|m| a.model_aliases.get(m)).filter(|id| *id != model.unwrap_or("")).cloned(),
        })
    }

    /// Whether any account could serve `provider` right now (no lease taken).
    pub fn any_available(&self, provider: &str, now: i64) -> bool {
        self.accounts.iter().any(|a| a.provider == provider && a.available(now))
    }

    /// Whether a sell-enabled account *other than* `except` could serve this
    /// lane right now.
    ///
    /// What the relay executor asks before handing a failed task to a second
    /// account. The market cannot ask it on our behalf: a lane is
    /// `{device}|{provider}`, so every `custom` account on this machine is one
    /// entry to the gateway and a single failure excludes all of them together.
    /// Which account served, and which others were standing by, is known only
    /// here.
    pub fn alternate_available(&self, provider: &str, model: &str, except: &str, now: i64) -> bool {
        // The dialect the lane was declared in still binds: the body in hand was
        // built for that wire, and an account speaking another is not a
        // substitute. Same rule as `acquire`.
        let wire = self.lane_wire(provider, model, now);
        self.accounts.iter().any(|a| {
            a.provider == provider
                && a.account_id != except
                && a.sell_enabled
                && !(wire.is_some() && a.wire_for(model).is_some() && a.wire_for(model) != wire)
                && a.lane_available(model, now)
        })
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
            // Both ride the same ladder and differ only in where it ends. A 5xx
            // that keeps coming back means this lane is broken; a refusal aimed
            // at the machine means the operator has a network to fix. Saying
            // which is the entire value of the pause the seller reads.
            UpstreamErrorKind::ServerError | UpstreamErrorKind::Blocked => {
                let stuck = match kind {
                    UpstreamErrorKind::Blocked => PauseReason::Blocked,
                    _ => PauseReason::Breaker,
                };
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
                        lane.paused = Some(stuck);
                        lane.resume_at = 0;
                        lane.cooldown_until = None;
                        Some(stuck)
                    }
                }
            }
            // One request is the whole evidence needed. Every later request for
            // this id would be answered the same way, so there is no ladder to
            // climb and nothing to be gained by letting two more buyers find
            // out — the lane comes off the market now and tries again tomorrow.
            UpstreamErrorKind::Unsupported => {
                let until = now + COOLDOWN_UNSUPPORTED_SECS;
                let lane = a.lanes.entry(model.to_string()).or_default();
                lane.last_error = detail.to_string();
                lane.paused = Some(PauseReason::Unsupported);
                lane.resume_at = until;
                // Deliberately no `cooldown_until`: that is the account's own
                // recovery ladder, and this says nothing about the account. Its
                // other models are unaffected and must keep selling.
                Some(PauseReason::Unsupported)
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
    ///
    /// An auth failure is the account's, not the lane's: `on_error` pauses
    /// *every* lane of the account for it, so clearing one lane and leaving the
    /// rest paused would leave the operator clicking "resume" once per model —
    /// dozens of times for a Codex account — to undo one signed-in-again. So an
    /// auth pause is cleared account-wide, together with the flag that put it
    /// there. A `Blocked` pause is cleared the same way and for the same reason:
    /// the upstream refused the *machine*, so every lane of the account laddered
    /// its way to the same pause and one fixed network releases all of them.
    /// Other reasons stay per-lane: a broken Opus lane says nothing about
    /// Sonnet.
    /// Returns every lane it actually cleared, so the caller can forget the
    /// matching persisted pauses and re-declare exactly those lanes — clearing
    /// more in memory than on disk would put them all back on the next restart.
    pub fn resume_lane(&mut self, provider: &str, account_id: &str, model: &str) -> Vec<String> {
        let Some(a) = self.find(provider, account_id) else { return Vec::new() };
        let wide = a
            .lanes
            .get(model)
            .and_then(|l| l.paused)
            .filter(|r| matches!(r, PauseReason::Auth | PauseReason::Blocked));
        a.cooldown_until = None;
        let mut cleared = Vec::new();
        if let Some(wide) = wide {
            if wide == PauseReason::Auth {
                a.auth_failed = false;
            }
            for (name, lane) in a.lanes.iter_mut() {
                if lane.paused == Some(wide) {
                    *lane = LaneState::default();
                    cleared.push(name.clone());
                }
            }
        }
        if let Some(lane) = a.lanes.get_mut(model) {
            *lane = LaneState::default();
            if !cleared.iter().any(|m| m == model) {
                cleared.push(model.to_string());
            }
        }
        cleared
    }

    /// Take back an auth failure because the credential behind it has been
    /// replaced. Returns the lanes it released.
    ///
    /// Signing in again is the operator doing the thing the pause asked for, so
    /// it has to be what clears it. It did not used to be: the pause survived
    /// the new login (the persisted rows are re-applied on every pool rebuild)
    /// and only the "resume" button cleared it, which left somebody who had just
    /// re-authenticated looking at a page still telling them to authenticate.
    ///
    /// Only `Auth` — a lane the breaker holds, or one switched off by hand, has
    /// nothing to do with the credential and is left exactly where it is.
    pub fn clear_auth_failure(&mut self, provider: &str, account_id: &str) -> Vec<String> {
        let Some(a) = self.find(provider, account_id) else { return Vec::new() };
        a.auth_failed = false;
        let mut cleared = Vec::new();
        for (name, lane) in a.lanes.iter_mut() {
            if lane.paused == Some(PauseReason::Auth) {
                *lane = LaneState::default();
                cleared.push(name.clone());
            }
        }
        cleared
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
        let lanes = self.accounts.iter().flat_map(|a| a.lanes.values()).filter_map(|l| l.auto_resume_at(now));
        // A spent subscription window is an account-wide clock, not a lane's,
        // and it is the one the operator is actually waiting on. Without it
        // here the whole account waits for the 60-second periodic rebuild to
        // stumble over a reset the provider named to the second.
        let quotas = self
            .accounts
            .iter()
            .filter(|a| a.quota_remaining == 0)
            .filter_map(|a| a.quota_reset_at)
            .filter(|t| *t > now);
        lanes.chain(quotas).min()
    }

    /// Every lane's current state, for the UI and for building the supply
    /// declaration.
    pub fn lane_views(&self, now: i64) -> Vec<LaneStatusView> {
        let mut out = Vec::new();
        for a in &self.accounts {
            for (model, lane) in &a.lanes {
                let (status, reason) = if let Some(r) = lane.pause_at(now) {
                    ("paused", Some(r))
                } else if lane.cooldown_until.is_some_and(|c| c > now) {
                    ("cooldown", None)
                } else if !a.sell_enabled || !a.sells_model(model) {
                    ("off", None)
                } else if !a.available(now) {
                    // The account itself is expired/exhausted; the lane is fine.
                    // Checked before the price band because a spent daily cap
                    // is the more actionable answer: it explains every lane of
                    // the account at once, and it is the one with a clock on it.
                    (a.status(now), None)
                } else if lane.price_withheld {
                    ("withheld", None)
                } else {
                    ("selling", None)
                };
                out.push(LaneStatusView {
                    provider: a.provider.clone(),
                    account_id: a.account_id.clone(),
                    model: model.clone(),
                    status: status.to_string(),
                    // A withheld lane carries `price` as its reason so both the
                    // UI and the supply declaration can say *why* without
                    // inventing a `PauseReason` for something that is not a
                    // pause. Nothing is broken, so `requires_user` stays false.
                    paused_reason: match (status, reason) {
                        (_, Some(r)) => Some(r.as_str().to_string()),
                        ("withheld", None) => Some("price".to_string()),
                        _ => None,
                    },
                    requires_user: reason.is_some_and(|r| r.requires_user()),
                    // A lane with no clock of its own inherits the account's,
                    // when the account is the thing holding it back: an
                    // exhausted subscription knows when it resets, and that is
                    // the countdown both the sell page and the gateway's
                    // paused-lane record should carry.
                    resume_at: match (lane.resume_at > now, status) {
                        (true, _) => lane.resume_at,
                        (false, "exhausted") => a.quota_reset_at.filter(|t| *t > now).unwrap_or(0),
                        _ => 0,
                    },
                    cooldown_until: lane.cooldown_until.filter(|c| *c > now),
                    fail_streak: lane.fail_streak,
                    last_error: lane.last_error.clone(),
                    // The lane's own switch, not the account's: a model the
                    // operator has left out of the selection is not for sale,
                    // and this is the field the supply declaration reads.
                    sell_enabled: a.sell_enabled && a.sells_model(model),
                    quota_remaining: a.quota_remaining,
                    ratio: lane.ratio,
                    min_ratio: a.sell_min_ratio,
                    max_ratio: a.sell_max_ratio,
                    concurrency_max: a.concurrency_max,
                    upstream_wire: a.wire_for(model),
                    upstream_base: a.upstream_base.clone(),
                });
            }
        }
        out
    }

    /// The dialect this device offers `model` in, where its accounts may each
    /// speak a different one.
    ///
    /// One market lane is one (model, device) pair and carries a single
    /// dialect, so a device holding endpoints of two protocols that both serve
    /// a model can only offer it in one of them. The dialect with the most
    /// headroom behind it wins — that keeps the largest endpoint sellable — and
    /// ties break on the dialect's own name, so the answer does not move
    /// between rebuilds and the declaration keeps agreeing with the pick.
    ///
    /// `None` when no account of this provider speaks a dialect of its own,
    /// which is every provider but `custom`.
    pub fn lane_wire(&self, provider: &str, model: &str, now: i64) -> Option<Wire> {
        let mut headroom: BTreeMap<&'static str, (i64, Wire)> = BTreeMap::new();
        let mut any: Option<Wire> = None;
        let mut responses_seen = false;
        for a in self.accounts.iter().filter(|a| a.provider == provider) {
            let (Some(w), true) = (a.wire_for(model), a.lanes.contains_key(model)) else {
                continue;
            };
            // Something to fall back on when every lane is paused: the offer is
            // still declared (with its reason), and it has to say *some*
            // dialect for the day it comes back.
            any.get_or_insert(w);
            if a.sell_enabled && a.lane_available(model, now) {
                responses_seen |= w == Wire::Responses;
                headroom.entry(w.as_str()).or_insert((0, w)).0 += a.quota_remaining as i64;
            }
        }
        // Headroom decides between two dialects that both work. For a model
        // that only one of them can serve (`needs_responses_wire`) there is
        // nothing to weigh: declaring it on the chat route because a bigger
        // endpoint speaks that one puts it back on the route that answers 400
        // to every tool call. The chat-only endpoints drop out of this lane and
        // keep selling every other model they have.
        //
        // Only counted among the endpoints actually serving right now: an
        // offline Responses endpoint must not take the lane away from one that
        // could serve it in a dialect of its own (a `claude`-wire relay, say).
        if responses_seen && needs_responses_wire(model) {
            return Some(Wire::Responses);
        }
        headroom
            .into_values()
            .max_by_key(|(h, w)| (*h, std::cmp::Reverse(w.as_str())))
            .map(|(_, w)| w)
            .or(any)
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
                quota_reset_at: a.quota_reset_at.filter(|t| *t > now && a.quota_remaining == 0),
                status: a.status(now).to_string(),
                expires_at: a.expires_at,
                cooldown_until: a.cooldown_until.filter(|c| *c > now),
                sell_enabled: a.sell_enabled,
                sell_models: a.sell_models.clone(),
                origin: a.origin.clone(),
                used_today: a.used_today,
                sell_daily_limit: a.sell_daily_limit,
                sell_min_ratio: a.sell_min_ratio,
                sell_max_ratio: a.sell_max_ratio,
                sell_concurrency: a.concurrency_max as i64,
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

    /// A sell-enabled account with a price band, and the prices to judge it.
    /// Both are percent *of* list price: 100 is list, 60 is four-tenths off.
    fn banded(id: &str, band: (i64, i64)) -> AccountRuntime {
        let mut a = acct(id, 1_000);
        a.sell_enabled = true;
        a.sell_min_ratio = band.0;
        a.sell_max_ratio = band.1;
        a
    }

    const LUNA: &str = "gpt-5.6-luna";

    /// A custom endpoint: OpenAI schema, selling one lite model and one
    /// ordinary one. `responses` is whether its host serves the second route.
    fn endpoint(id: &str, quota: u64, responses: bool) -> AccountRuntime {
        let mut a = AccountRuntime::new("custom", id, &format!("custom:{id}")).with_models([LUNA, "gpt-5.5"]);
        a.quota_remaining = quota;
        a.sell_enabled = true;
        a.concurrency_max = 4;
        a.upstream_base = Some("https://relay.example/v1".into());
        a.upstream_wire = Some(Wire::Openai);
        a.upstream_responses = responses;
        a
    }

    /// Several `custom` accounts on one machine are a single lane to the
    /// market, so a failed task can only be handed to another one from here.
    /// The question has to exclude the account that just failed and honour the
    /// dialect the lane was declared in.
    #[test]
    fn another_account_on_this_device_can_take_over_a_failed_lane() {
        let mut pool = AccountPool::new(Strategy::RoundRobin);
        pool.set_accounts(vec![endpoint("openrouter", 1_000, true), endpoint("bai-mix6", 1_000, true)]);
        assert!(pool.alternate_available("custom", "gpt-5.5", "openrouter", 0));
        // Nobody else sells it: the buyer hears the original failure instead of
        // the same account being tried twice.
        let mut alone = AccountPool::new(Strategy::RoundRobin);
        alone.set_accounts(vec![endpoint("openrouter", 1_000, true)]);
        assert!(!alone.alternate_available("custom", "gpt-5.5", "openrouter", 0));
        // A lane the other account cannot speak the dialect of is not a
        // substitute — `LUNA` is Responses-only and `bai-chat` is chat-only.
        let mut mixed = AccountPool::new(Strategy::RoundRobin);
        mixed.set_accounts(vec![endpoint("openrouter", 1_000, true), endpoint("bai-chat", 1_000, false)]);
        assert!(!mixed.alternate_available("custom", LUNA, "openrouter", 0));
    }

    #[test]
    fn one_endpoint_offers_the_lite_models_on_the_route_that_can_serve_them() {
        let a = endpoint("mix", 1_000, true);
        assert_eq!(a.wire_for(LUNA), Some(Wire::Responses));
        // Everything else stays where its aliases and its tool schema work.
        assert_eq!(a.wire_for("gpt-5.5"), Some(Wire::Openai));
        // An endpoint without the second route claims nothing it cannot do.
        assert_eq!(endpoint("chat-only", 1_000, false).wire_for(LUNA), Some(Wire::Openai));
    }

    /// The generation is not uniform, and the difference is the whole point of
    /// naming them: `gpt-5.6-codex` is a full Responses model that serves the
    /// chat route, so a chat-only endpoint may sell it.
    #[test]
    fn only_the_lite_models_are_kept_off_the_chat_route() {
        for lite in ["gpt-5.6-sol", "gpt-5.6-luna", "gpt-5.6-terra"] {
            assert!(needs_responses_wire(lite), "{lite}");
        }
        for ok in ["gpt-5.6-codex", "gpt-5.5", "claude-opus-5", "gpt-5.6"] {
            assert!(!needs_responses_wire(ok), "{ok} was wrongly kept off the chat route");
        }
    }

    #[test]
    fn a_lite_lane_is_declared_on_responses_even_against_more_chat_headroom() {
        // Headroom is the tie-break between two dialects that both work. Here
        // only one does, so the bigger chat-only endpoint must not drag the
        // lane back onto the route that 400s every tool call.
        let mut pool = AccountPool::new(Strategy::RoundRobin);
        pool.set_accounts(vec![endpoint("small", 10, true), endpoint("big", 10_000, false)]);
        assert_eq!(pool.lane_wire("custom", LUNA, 0), Some(Wire::Responses));
        // The same two endpoints agree on every other model, so nothing there
        // is left out.
        assert_eq!(pool.lane_wire("custom", "gpt-5.5", 0), Some(Wire::Openai));
    }

    #[test]
    fn a_lease_for_a_lite_model_carries_the_route_it_will_be_sent_on() {
        let mut pool = AccountPool::new(Strategy::RoundRobin);
        pool.set_accounts(vec![endpoint("mix", 1_000, true)]);
        // What the executor reads to build the URL: a pick that said "openai"
        // here would post a Responses body to /chat/completions.
        let picked = pool.pick_for_sale("custom", LUNA, Some(Wire::Responses), 0).expect("a lane");
        assert_eq!(picked.upstream_wire, Some(Wire::Responses));
        // And the chat-only endpoint is not eligible for that declaration.
        let mut pool = AccountPool::new(Strategy::RoundRobin);
        pool.set_accounts(vec![endpoint("chat-only", 1_000, false)]);
        assert!(pool.pick_for_sale("custom", LUNA, Some(Wire::Responses), 0).is_none());
    }

    fn prices(pairs: &[(&str, i32)]) -> BTreeMap<String, i32> {
        pairs.iter().map(|(m, r)| (m.to_string(), *r)).collect()
    }

    #[test]
    fn a_full_lease_table_is_told_apart_from_a_lane_that_cannot_serve() {
        // One account's ceiling is a budget shared by every model it sells, but
        // it is declared to the market once per lane — so the gateway is
        // entitled to send this account `concurrency_max` tasks for *each* of
        // its models. The difference between "come back in a moment" and "this
        // lane cannot serve you" is the whole of what the executor needs to know
        // to avoid reporting the first as a broken credential.
        let now = 1_000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        let mut a = banded("a", (5, 100));
        a.concurrency_max = 2;
        p.set_accounts(vec![a]);

        assert!(!p.lane_saturated("claude", OPUS, None, now), "an idle account is not saturated");
        for _ in 0..2 {
            assert!(p.pick_for_sale("claude", OPUS, None, now).is_some());
        }
        // Both slots are out on Opus, and the third caller is refused — but this
        // is the account being busy, and Haiku, which shares that same budget,
        // is just as busy despite not having served anything.
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none());
        assert!(p.lane_saturated("claude", OPUS, None, now));
        assert!(p.pick_for_sale("claude", HAIKU, None, now).is_none());
        assert!(p.lane_saturated("claude", HAIKU, None, now));

        // A model this account does not sell has nothing to wait for, and
        // neither does a lane whose account is out of the market for a reason
        // that will not clear on its own.
        assert!(!p.lane_saturated("claude", "claude-sonnet-5", None, now));
        let mut broken = AccountPool::new(Strategy::RoundRobin);
        broken.set_accounts(vec![{
            let mut a = banded("a", (5, 100));
            a.concurrency_max = 2;
            a.in_use = 2;
            a.auth_failed = true;
            a
        }]);
        assert!(
            !broken.lane_saturated("claude", OPUS, None, now),
            "an expired credential is not a queue: waiting on it would never end"
        );
    }

    #[test]
    fn price_outside_the_band_withholds_the_lane_at_once() {
        let now = 1_000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![banded("a", (60, 100))]);
        // Opus is trading at 38% of list — below what this account will accept
        // — while Haiku is at 80% and inside the band.
        p.apply_prices(&prices(&[(OPUS, 38), (HAIKU, 80)]));

        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none(), "withheld lane must not sell");
        assert!(p.pick_for_sale("claude", HAIKU, None, now).is_some());

        let views = p.lane_views(now);
        let opus = views.iter().find(|v| v.model == OPUS).unwrap();
        assert_eq!(opus.status, "withheld");
        assert_eq!(opus.paused_reason.as_deref(), Some("price"));
        assert!(!opus.requires_user, "a price decision is not something to fix");
        assert_eq!(opus.ratio, Some(38));
        // Local use of your own subscription is not a sale, so the band on it
        // must not lock the operator out.
        assert!(p.pick_local("claude", OPUS, now).is_some());
    }

    #[test]
    fn a_recovered_price_puts_the_lane_back_on_the_next_tick() {
        let now = 1_000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![banded("a", (60, 100))]);
        p.apply_prices(&prices(&[(OPUS, 38)]));
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none());

        // Back inside the band, but only just: the margin keeps a price parked
        // on the edge from flapping the lane on and off.
        p.apply_prices(&prices(&[(OPUS, 60)]));
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none(), "on the edge is not back");

        // Clear of the edge — and that is the whole condition. Waiting the
        // price out cost sellers the peak they were waiting for: the server
        // reprices every minute and a peak lasts one or two.
        p.apply_prices(&prices(&[(OPUS, 70)]));
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_some());
    }

    #[test]
    fn a_floor_at_the_ceiling_still_sells_at_the_ceiling() {
        // What a metered endpoint reselling at cost has to charge: 100% of list
        // and not a point less. The band has zero width, so the margin is zero
        // too — "exactly my floor" has to mean sell, or this seller can never
        // trade at all.
        let now = 1_000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![banded("a", (100, 100))]);
        p.apply_prices(&prices(&[(OPUS, 100)]));
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_some(), "at the floor is selling");

        // Demand eases, the price comes off the ceiling: withheld at once.
        p.apply_prices(&prices(&[(OPUS, 73)]));
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none());

        // And back the moment it returns — this is the case the old dwell made
        // unreachable, because the price never held at 100% for the three to
        // six minutes it demanded.
        p.apply_prices(&prices(&[(OPUS, 100)]));
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_some(), "the peak is short; catch it");
    }

    #[test]
    fn widening_the_band_puts_the_lane_back_at_once() {
        let now = 1_000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![banded("a", (60, 100))]);
        p.apply_prices(&prices(&[(OPUS, 38)]));
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none());

        // The operator lowers their own floor. That is a decision about what
        // they will accept, not a market move, so it clears the hysteresis
        // outright rather than being judged against the edge they just left.
        p.set_accounts(vec![banded("a", (20, 100))]);
        p.apply_prices(&prices(&[(OPUS, 38)]));
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_some());
    }

    /// Narrowing which models an account sells takes exactly those lanes off
    /// the market and leaves the rest of the subscription trading, while the
    /// operator's own local use of it is untouched — a sale is not the same
    /// thing as using the subscription you pay for.
    #[test]
    fn a_narrowed_account_sells_only_the_models_it_was_narrowed_to() {
        let now = 1_000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        let mut a = acct("a", 1_000);
        a.sell_enabled = true;
        a.sell_models = vec![HAIKU.to_string()];
        p.set_accounts(vec![a]);

        assert!(p.pick_for_sale("claude", HAIKU, None, now).is_some());
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none(), "left out of the selection");
        // Still visible on the sell page, and switched off there rather than
        // vanished: an invisible lane is one nobody can switch back on.
        let opus = p.lane_views(now).into_iter().find(|v| v.model == OPUS).expect("still listed");
        assert_eq!(opus.status, "off");
        assert!(!opus.sell_enabled, "so the declaration leaves it out");
        // The local route ignores the sell selection entirely.
        assert!(p.pick_local("claude", OPUS, now).is_some());
    }

    #[test]
    fn an_unknown_price_never_withholds() {
        let now = 1_000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![banded("a", (60, 100))]);
        p.apply_prices(&prices(&[(OPUS, 38)]));
        // The market went unreachable: being offline is not a reason to stop
        // selling on the terms this device already had.
        p.apply_prices(&BTreeMap::new());
        assert!(p.pick_for_sale("claude", OPUS, None, now + 10).is_some());
        assert_eq!(p.lane_views(now + 10)[0].ratio, None);
    }

    #[test]
    fn the_default_band_withholds_nothing() {
        let now = 1_000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        let mut a = acct("a", 1_000);
        a.sell_enabled = true;
        p.set_accounts(vec![a]);
        // The two ends of what the server's `mkt_ratio` clamp allows.
        p.apply_prices(&prices(&[(OPUS, 10), (HAIKU, 100)]));
        assert!(p.lane_views(now).iter().all(|v| v.status == "selling"));
    }

    #[test]
    fn a_spent_daily_cap_outranks_the_price_band() {
        let now = 1_000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        // Quota 0 is what `rebuild_pool` leaves behind once the daily sell cap
        // is spent; the operator needs to hear about that, not about the price.
        let mut a = banded("a", (60, 100));
        a.quota_remaining = 0;
        p.set_accounts(vec![a]);
        p.apply_prices(&prices(&[(OPUS, 38), (HAIKU, 38)]));
        assert!(p.lane_views(now).iter().all(|v| v.status == "exhausted"));
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
        assert_eq!(p.pick_for_sale("claude", OPUS, None, now).unwrap().account_id, "asale-owned");
        assert!(p.any_sellable("claude", OPUS, now));
        // …while the local direct route may still use either.
        assert!(p.pick("claude", now).is_some());

        // Switching the only sellable account off stops sales but not local use.
        let mut both_off = acct("asale-owned", 100);
        both_off.sell_enabled = false;
        p.set_accounts(vec![both_off]);
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none());
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
        p.pick_for_sale("claude", OPUS, None, now).unwrap();
        p.on_error("claude", "a", OPUS, UpstreamErrorKind::ServerError, "500", now);

        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none(), "the failed lane is cooling");
        assert!(p.pick_for_sale("claude", HAIKU, None, now).is_some(), "its sibling keeps selling");
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
            p.pick_for_sale("claude", OPUS, None, at).unwrap();
            assert_eq!(p.on_error("claude", "a", OPUS, UpstreamErrorKind::ServerError, "500", at), None);
            assert!(p.pick_for_sale("claude", OPUS, None, at + rung - 1).is_none(), "rung {i} still cooling");
            at += rung + 1;
            assert!(p.pick_for_sale("claude", OPUS, None, at).is_some(), "rung {i} must clear on its own");
            // That pick leased a slot; hand it back without clearing the streak
            // (only a success does that).
            p.find("claude", "a").unwrap().in_use -= 1;
        }

        // The third consecutive failure is not a bad minute — it is a broken
        // lane, and it stays out until someone looks at it.
        p.pick_for_sale("claude", OPUS, None, at).unwrap();
        assert_eq!(
            p.on_error("claude", "a", OPUS, UpstreamErrorKind::ServerError, "500", at),
            Some(PauseReason::Breaker)
        );
        assert_eq!(p.find("claude", "a").unwrap().lanes[OPUS].fail_streak, LANE_BREAKER_THRESHOLD);
        assert!(p.pick_for_sale("claude", OPUS, None, at + 86_400).is_none(), "a breaker does not time out");

        let view = p.lane_views(at).into_iter().find(|v| v.model == OPUS).unwrap();
        assert_eq!(view.status, "paused");
        assert_eq!(view.paused_reason.as_deref(), Some("breaker"));
        assert!(view.requires_user, "the UI must offer a resume button for this");

        // …and the operator's resume puts it straight back.
        p.resume_lane("claude", "a", OPUS);
        assert!(p.pick_for_sale("claude", OPUS, None, at).is_some());
        assert_eq!(p.find("claude", "a").unwrap().lanes[OPUS].fail_streak, 0);
    }

    #[test]
    fn a_success_resets_the_ladder() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pick_for_sale("claude", OPUS, None, now).unwrap();
        p.on_error("claude", "a", OPUS, UpstreamErrorKind::ServerError, "500", now);
        let later = now + LANE_COOLDOWN_LADDER[0] + 1;
        p.pick_for_sale("claude", OPUS, None, later).unwrap();
        p.on_success("claude", "a", OPUS, 100);
        assert_eq!(p.find("claude", "a").unwrap().lanes[OPUS].fail_streak, 0);
        // So the next isolated failure starts at the first rung again, rather
        // than landing one step from the breaker.
        p.pick_for_sale("claude", OPUS, None, later + 1).unwrap();
        assert_eq!(
            p.on_error("claude", "a", OPUS, UpstreamErrorKind::ServerError, "500", later + 1),
            None
        );
    }

    #[test]
    fn a_blocked_machine_is_backed_off_and_then_says_so() {
        let mut now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        // The first refusals are treated as a proxy having a bad minute: the
        // lane backs off, nobody is told anything, and it comes back by itself.
        for rung in LANE_COOLDOWN_LADDER {
            p.pick_for_sale("claude", OPUS, None, now).unwrap();
            assert_eq!(
                p.on_error("claude", "a", OPUS, UpstreamErrorKind::Blocked, "403 forbidden", now),
                None
            );
            now += rung + 1;
        }
        // Out of rungs, and this is where it used to become "sign in again".
        p.pick_for_sale("claude", OPUS, None, now).unwrap();
        assert_eq!(
            p.on_error("claude", "a", OPUS, UpstreamErrorKind::Blocked, "403 forbidden", now),
            Some(PauseReason::Blocked)
        );
        // The account's credential is untouched: nothing here says the login is
        // bad, so the account must not read as expired.
        assert!(!p.find("claude", "a").unwrap().auth_failed);
        assert_eq!(p.find("claude", "a").unwrap().status(now), "available");
        let view = p.lane_views(now).into_iter().find(|v| v.model == OPUS).unwrap();
        assert_eq!(view.paused_reason.as_deref(), Some("blocked"));
        assert!(view.requires_user, "a network the operator has to fix needs a person");
    }

    /// A model the upstream has never heard of comes off the market on the
    /// first request rather than after three, and takes nothing else with it.
    #[test]
    fn a_model_the_upstream_does_not_have_stops_being_offered_at_once() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pick_for_sale("claude", OPUS, None, now).unwrap();
        assert_eq!(
            p.on_error("claude", "a", OPUS, UpstreamErrorKind::Unsupported, "404 not_found_error", now),
            Some(PauseReason::Unsupported),
            "no ladder: the second buyer would get the same 404 as the first"
        );
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none());
        assert!(!p.any_sellable("claude", OPUS, now));

        // The account is fine, and so is everything else it sells.
        assert!(!p.find("claude", "a").unwrap().auth_failed);
        assert_eq!(p.find("claude", "a").unwrap().status(now), "available");
        assert!(p.pick_for_sale("claude", HAIKU, None, now).is_some());

        let view = p.lane_views(now).into_iter().find(|v| v.model == OPUS).unwrap();
        assert_eq!(view.paused_reason.as_deref(), Some("unsupported"));
        assert!(!view.requires_user, "there is nothing for the operator to do about a retired model");

        // And it tries once more a day later, in case the vendor brings it back.
        assert_eq!(p.next_auto_resume(now), Some(now + COOLDOWN_UNSUPPORTED_SECS));
        assert!(p.pick_for_sale("claude", OPUS, None, now + COOLDOWN_UNSUPPORTED_SECS + 1).is_some());
    }

    /// A pause with a resume instant is over when that instant passes.
    ///
    /// It used to take a *successful* call on the same lane to clear one — work
    /// a paused lane is never sent — so on a device that only sells, a single
    /// 429 parked a model until the daemon was restarted.
    #[test]
    fn a_self_clearing_pause_is_over_when_its_instant_passes() {
        let now = 1000;
        let reset = now + 600;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pick_for_sale("claude", OPUS, None, now).unwrap();
        p.on_error("claude", "a", OPUS, UpstreamErrorKind::RateLimited { reset_at: Some(reset) }, "429", now);
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none());

        assert!(p.pick_for_sale("claude", OPUS, None, reset + 1).is_some(), "the reset it named has passed");
        let view = p.lane_views(reset + 1).into_iter().find(|v| v.model == OPUS).unwrap();
        assert_eq!(view.status, "selling");

        // A pause with no instant to come back at is not touched by the clock:
        // it waits for the operator, or for the catalog.
        p.pause_lane("claude", "a", HAIKU, PauseReason::Manual, 0);
        assert!(p.pick_for_sale("claude", HAIKU, None, reset + 86_400).is_none());
    }

    #[test]
    fn signing_in_again_is_what_clears_an_auth_pause() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pick_for_sale("claude", OPUS, None, now).unwrap();
        assert_eq!(
            p.on_error("claude", "a", OPUS, UpstreamErrorKind::AuthFailed, "401", now),
            Some(PauseReason::Auth)
        );
        assert_eq!(p.find("claude", "a").unwrap().status(now), "expired");

        // The operator does the one thing the pause asked for. It used to take a
        // separate "resume" click as well, because a fresh login cleared nothing
        // — so the page went on telling somebody who had just signed in that they
        // needed to sign in.
        let cleared = p.clear_auth_failure("claude", "a");
        assert!(cleared.contains(&OPUS.to_string()));
        assert!(!p.find("claude", "a").unwrap().auth_failed);
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_some());
    }

    #[test]
    fn a_fresh_credential_leaves_a_hand_paused_lane_alone() {
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pause_lane("claude", "a", OPUS, PauseReason::Manual, 0);
        assert!(p.clear_auth_failure("claude", "a").is_empty());
        assert_eq!(
            p.find("claude", "a").unwrap().lanes[OPUS].paused,
            Some(PauseReason::Manual),
            "re-authenticating says nothing about a lane its owner switched off"
        );
    }

    #[test]
    fn a_rate_limit_resumes_itself_at_the_reset() {
        let now = 1000;
        let reset = now + 600;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pick_for_sale("claude", OPUS, None, now).unwrap();
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
        p.pick_for_sale("claude", OPUS, None, now).unwrap();
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
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_some());
        assert!(p.lane_views(now).iter().all(|v| v.paused_reason.is_none()));
    }

    /// One sign-in, one click. `on_error` pauses every lane of the account for
    /// an auth failure, so resuming one lane has to undo all of them — otherwise
    /// the operator faces a "resume" button per model (a Codex account sells
    /// dozens) to clear a single event.
    #[test]
    fn resuming_one_lane_clears_the_whole_auth_pause() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.on_error("claude", "a", OPUS, UpstreamErrorKind::AuthFailed, "401", now);
        p.resume_lane("claude", "a", OPUS);
        assert!(p.lane_views(now).iter().all(|v| v.paused_reason.is_none()), "every lane comes back");
        assert!(p.pick_for_sale("claude", HAIKU, None, now).is_some(), "a lane nobody named still serves");
        assert_eq!(p.statuses(now)[0].status, "available", "the account-level flag goes too");
    }

    /// A pause that is genuinely about one lane must not be cleared wholesale by
    /// resuming a different one.
    #[test]
    fn resuming_a_lane_leaves_other_lanes_own_pauses_alone() {
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pause_lane("claude", "a", HAIKU, PauseReason::Breaker, 0);
        p.resume_lane("claude", "a", OPUS);
        assert!(p.pick_for_sale("claude", HAIKU, None, now).is_none());
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_some());
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
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none());
        assert!(p.pick_for_sale("claude", HAIKU, None, now).is_some());
    }

    #[test]
    fn local_use_is_unaffected_by_lane_pauses() {
        // Pauses are about selling. Your own subscription, used locally, is not
        // a sale — a breaker on the market side must not lock you out of it.
        let now = 1000;
        let mut p = AccountPool::new(Strategy::RoundRobin);
        p.set_accounts(vec![sellable("a")]);
        p.pause_lane("claude", "a", OPUS, PauseReason::Breaker, 0);
        assert!(p.pick_for_sale("claude", OPUS, None, now).is_none());
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
