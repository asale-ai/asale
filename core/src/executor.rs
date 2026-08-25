//! Provider executor (spec §5.2). On an `http_request`, inject the local
//! subscription token, call the upstream, and stream `stream_start/chunk/end`
//! frames back. The token is injected here and nowhere else — the server and
//! consumer never see it.

use crate::protocol::{self, Envelope, HttpRequestPayload, Usage};
use asale_protocol::ids::{Provider, Wire};
use crate::security::QuotaVerifier;
use async_trait::async_trait;
use base64::Engine;
use futures_util::StreamExt;
use serde_json::json;
use std::collections::BTreeMap;
use sha2::Digest as _Sha2Digest;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The same request body with its `model` replaced.
///
/// `None` when the body is not a JSON object — which would mean the gateway
/// built something this path cannot speak for, and guessing at it is worse than
/// saying so.
fn with_model(body: &[u8], model_id: &str) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.as_object_mut()?.insert("model".into(), serde_json::Value::String(model_id.to_string()));
    serde_json::to_vec(&v).ok()
}

/// The endpoint under a custom account's base URL that serves `wire`.
///
/// The base is what its operator pasted, so both spellings people actually use
/// are accepted: with the `/v1` suffix (`https://host/api/v1`, what a vendor's
/// docs print) and without it. A base that already names the endpoint is left
/// alone rather than having a second copy appended.
///
/// `built` is the URL the gateway produced, and only Gemini needs it: that
/// dialect puts the model *and* whether the call streams into the path
/// (`/models/{model}:streamGenerateContent?alt=sse`), so the tail the gateway
/// built is what carries the request and only the origin is ours to replace.
fn custom_url(base: &str, wire: Wire, built: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    // Between the two OpenAI routes the gateway's path wins over the recorded
    // dialect. It built the *body* for one of them and `custom_placeholder`
    // keeps that path, while this side's record can be a rebuild stale — and
    // posting a Responses body to `/chat/completions` is a 400 nobody can read.
    // Both routes take the same bearer, so nothing else moves with it, and the
    // other two dialects have no second route to confuse this way.
    let path = built.split('?').next().unwrap_or(built);
    let wire = match wire {
        Wire::Openai | Wire::Responses if path.ends_with("/responses") => Wire::Responses,
        Wire::Openai | Wire::Responses if path.ends_with("/chat/completions") => Wire::Openai,
        w => w,
    };
    let join = |suffix: &str| {
        if base.ends_with(suffix) {
            base.to_string()
        } else {
            format!("{base}/{suffix}")
        }
    };
    match wire {
        Wire::Openai => join("chat/completions"),
        Wire::Responses => join("responses"),
        Wire::Claude => join("messages"),
        Wire::Gemini => match built.find("/models/") {
            Some(i) => format!("{base}{}", &built[i..]),
            // A built URL with no model in it is one this path cannot complete.
            // Addressing the collection is wrong but reaches the operator's own
            // host, where it fails as a 404 they can read — better than sending
            // the placeholder, which fails as DNS.
            None => join("models"),
        },
    }
}

/// The header a custom endpoint expects its key in.
///
/// Not a matter of taste: an Anthropic-compatible host ignores a bearer and
/// answers 401 for the missing `x-api-key`, and Google's wants `x-goog-api-key`.
/// This is only ever applied to a custom account — a *subscription* is a bearer
/// whatever its upstream's dialect, Anthropic's own OAuth included.
fn authorize_custom(
    builder: reqwest::RequestBuilder,
    wire: Wire,
    token: &str,
    headers: &serde_json::Map<String, serde_json::Value>,
) -> reqwest::RequestBuilder {
    match wire {
        Wire::Openai | Wire::Responses => builder.header("authorization", format!("Bearer {token}")),
        Wire::Claude => {
            let b = builder.header("x-api-key", token);
            // The gateway's Claude builder sends this already; a proxy that
            // rewrites headers is the case worth covering, and a required
            // header missing costs the whole sale.
            if headers.keys().any(|k| k.eq_ignore_ascii_case("anthropic-version")) {
                b
            } else {
                b.header("anthropic-version", ANTHROPIC_VERSION)
            }
        }
        Wire::Gemini => builder.header("x-goog-api-key", token),
    }
}

/// The Messages API version an Anthropic-compatible host is addressed with.
/// Mirrors the gateway's own (`translator::claude::ANTHROPIC_VERSION`).
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The line Anthropic requires at the head of a subscription request's system
/// prompt (see `with_claude_code_system`).
const CLAUDE_CODE_SYSTEM: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Beta flag Claude Code sends with OAuth tokens.
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";

/// The beta that makes Anthropic read the request as Claude Code at all. Its
/// absence — not the system prompt's — is what put subscription traffic on the
/// "third-party apps draw from your extra usage" path.
const CLAUDE_CODE_BETA: &str = "claude-code-20250219";

/// The betas Claude Code declares on every `/v1/messages` call, in wire order.
/// Mirrors CLIProxyAPI's `claudeCodeCLIBetas`.
///
/// `oauth` says the bearer is a subscription rather than an API key; the three
/// credential-scoped entries ride on it.
///
/// One deliberate departure from the CLI profile: `redact-thinking-2026-02-12`
/// is only declared when the body has no thinking enabled. Anthropic honours
/// the redaction by returning thinking blocks with an empty `thinking` field,
/// and on this path the reasoning is content a buyer paid for. The CLI itself
/// drops the beta whenever it asks for thinking summaries, so the shape is one
/// a real client does send.
pub fn claude_code_betas(body: &[u8], oauth: bool) -> String {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
    let mut betas: Vec<&str> = Vec::with_capacity(12);
    betas.push(CLAUDE_CODE_BETA);
    if oauth {
        betas.push(CLAUDE_OAUTH_BETA);
    }
    betas.push("interleaved-thinking-2025-05-14");
    let thinking_on = v.pointer("/thinking/type").and_then(|t| t.as_str()) == Some("enabled")
        || v.pointer("/thinking/budget_tokens").is_some();
    if !thinking_on {
        betas.push("redact-thinking-2026-02-12");
    }
    betas.push("thinking-token-count-2026-05-13");
    betas.push("context-management-2025-06-27");
    betas.push("prompt-caching-scope-2026-01-05");
    if v.get("tools").and_then(|t| t.as_array()).is_some_and(|t| !t.is_empty()) {
        betas.push("advanced-tool-use-2025-11-20");
    }
    betas.push("effort-2025-11-24");
    if oauth {
        betas.push("fallback-credit-2026-06-01");
    }
    // `speed` is rejected outright as an unknown field unless this is declared.
    if v.get("speed").and_then(|s| s.as_str()).is_some_and(|s| s.eq_ignore_ascii_case("fast")) {
        betas.push("fast-mode-2026-02-01");
    }
    if oauth {
        betas.push("extended-cache-ttl-2025-04-11");
    }
    betas.join(",")
}

/// The much smaller beta profile Claude Code sends to
/// `/v1/messages/count_tokens`: no redact-thinking, no prompt-caching-scope, no
/// effort, and none of the body-dependent flags. Mirrors CLIProxyAPI's
/// `claudeCountTokensBetasForCredential`.
pub fn claude_code_count_tokens_betas(oauth: bool) -> String {
    let mut betas = vec![CLAUDE_CODE_BETA];
    if oauth {
        betas.push(CLAUDE_OAUTH_BETA);
    }
    betas.extend(["interleaved-thinking-2025-05-14", "context-management-2025-06-27", "token-counting-2024-11-01"]);
    betas.join(",")
}

/// Header names a Claude subscription request owns outright, so whatever the
/// gateway put there is dropped before [`claude_identity_headers`] re-emits it.
const CLAUDE_IDENTITY_HEADERS: &[&str] = &[
    "anthropic-beta",
    "anthropic-dangerous-direct-browser-access",
    "user-agent",
    "x-app",
    "x-stainless-retry-count",
    "x-stainless-runtime",
    "x-stainless-lang",
    "x-stainless-timeout",
    "x-stainless-package-version",
    "x-stainless-runtime-version",
    "x-stainless-os",
    "x-stainless-arch",
    "x-claude-code-session-id",
    "x-client-request-id",
];

/// The fixed header identity Claude Code 2.1.220 (`@anthropic-ai/sdk` 0.94.0)
/// puts on every Messages call. Mirrors CLIProxyAPI's `identityHeader` block.
///
/// The body fingerprint alone is not what Anthropic reads: a request claiming
/// `cc_version=2.1.220` from a `claude-cli/1.0` user-agent with none of the SDK
/// headers is exactly the mismatch that reads as a third-party client.
pub fn claude_identity_headers(session_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("user-agent", asale_protocol::spec(Provider::Claude).user_agent.to_string()),
        ("anthropic-dangerous-direct-browser-access", "true".into()),
        ("x-app", "cli".into()),
        ("x-stainless-retry-count", "0".into()),
        ("x-stainless-runtime", "node".into()),
        ("x-stainless-lang", "js".into()),
        ("x-stainless-timeout", "600".into()),
        ("x-stainless-package-version", CLAUDE_SDK_VERSION.into()),
        ("x-stainless-runtime-version", CLAUDE_NODE_VERSION.into()),
        ("x-stainless-os", "MacOS".into()),
        ("x-stainless-arch", "arm64".into()),
        ("x-claude-code-session-id", session_uuid(session_id)),
        ("x-client-request-id", uuid::Uuid::new_v4().to_string()),
    ]
}

/// The `@anthropic-ai/sdk` and Node versions Claude Code 2.1.220 ships with.
const CLAUDE_SDK_VERSION: &str = "0.94.0";
const CLAUDE_NODE_VERSION: &str = "v26.3.0";

/// A leased token: the bearer plus the pool account it came from (empty
/// account_id when the provider has no pool semantics).
#[derive(Debug, Clone, Default)]
pub struct LeasedToken {
    pub token: String,
    pub account_id: String,
    /// The session id the upstream knows this account by, where clinging is
    /// part of the bargain. Only Claude uses it so far: the executor derives
    /// one stable id per serving account the first time it leases that
    /// account, so every request the account serves is attributed to one
    /// long-running Claude Code session rather than to a fresh third-party
    /// client each call.
    pub session_id: Option<String>,
    /// The id the vendor knows this subscription by, when its upstream demands
    /// that id next to the bearer. Only Codex uses it so far — see the
    /// `chatgpt-account-id` block in [`execute`].
    pub upstream_account_id: Option<String>,
    /// The endpoint this account's requests go to, for a `custom` account whose
    /// host the gateway cannot know. `None` means "use the URL the gateway
    /// built", which is every other provider.
    pub upstream_base: Option<String>,
    /// The dialect that endpoint speaks, which decides both the path under
    /// `upstream_base` and the header the key travels in. Only read when
    /// `upstream_base` is set; `None` there means the OpenAI schema, which is
    /// what every custom account spoke before this was a choice.
    pub upstream_wire: Option<Wire>,
    /// The id this account's upstream knows the requested model by, when it
    /// differs from the market's. `None` means "send the model id as it
    /// arrived" — true of every provider whose ids the catalog stores natively.
    pub upstream_model: Option<String>,
}

/// Outcome of one upstream call, reported back so a pool can release the
/// concurrency slot and apply cooldowns (spec §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    /// Completed (including non-account-fault 4xx); `tokens_used` decays quota.
    Success { tokens_used: u64 },
    /// Upstream 429; `reset_at` from Retry-After when present (unix secs).
    RateLimited { reset_at: Option<i64> },
    /// Upstream 5xx or transport failure.
    ServerError,
    /// Upstream 401/403 — token invalid.
    AuthFailed,
    /// Upstream 403 aimed at the machine rather than the credential — a region
    /// block or a middlebox. See [`refusal_outcome`].
    Blocked,
    /// The upstream has never heard of this model id. See [`unsupported_model`].
    Unsupported,
    /// The account's extra-usage allowance is spent. Cooled exactly like a 429,
    /// because that is what it is; kept separate so the seller is told to top
    /// up rather than to wait out a window that will never reset on its own.
    /// See [`quota_exhausted`].
    QuotaExhausted { reset_at: Option<i64> },
}

/// Which of the two very different things a 401/403 can mean.
///
/// A 401 is always the credential: every provider here answers a bad or expired
/// bearer with one. A 403 is not — Anthropic serves
/// `{"type":"forbidden","message":"Request not allowed"}` to a request coming
/// from a region it does not sell to, and OpenAI has
/// `unsupported_country_region_territory` for the same situation. Both arrive
/// with a login that is perfectly valid.
///
/// Getting this wrong is not a cosmetic mistake. `AuthFailed` takes the whole
/// account off the market and tells its owner to sign in again — advice that
/// cannot work, about a credential that is not broken, while the thing that is
/// (their network route to the vendor) goes unmentioned. So a 403 has to earn
/// `AuthFailed` by saying something about permissions or keys; anything else it
/// says, including nothing at all, reads as the machine being refused.
pub fn refusal_outcome(status: u16, body: &str) -> TaskOutcome {
    if status != 403 {
        return TaskOutcome::AuthFailed;
    }
    let b = body.to_ascii_lowercase();
    // Deliberately narrow. These are the words a provider uses when it is
    // talking about the *credential*; a geo refusal never contains them.
    const CREDENTIAL_MARKERS: [&str; 7] = [
        "authentication_error",
        "permission_error",
        "invalid_api_key",
        "invalid_token",
        "expired",
        "api key",
        "oauth",
    ];
    if CREDENTIAL_MARKERS.iter().any(|m| b.contains(m)) {
        TaskOutcome::AuthFailed
    } else {
        TaskOutcome::Blocked
    }
}

/// Whether a 4xx says the *model* does not exist, as opposed to anything else
/// wrong with the request.
///
/// This is the one 4xx that must not be shrugged off. The rest are the
/// consumer's problem — a malformed body, an oversized prompt — and the lane
/// that relayed them is healthy, which is why a 4xx otherwise costs the lane
/// nothing. "No such model" is a fact about what this account can serve, and it
/// will be just as true for the next buyer: on 2026-08-17 four Anthropic ids
/// the platform still listed (`claude-3-haiku`, `claude-opus-4`,
/// `claude-opus-4-1`, `claude-sonnet-4`) were answered `404 not_found_error` by
/// every subscription that was asked, and every buyer who picked one failed,
/// because nothing in the seller took the id out of what it was advertising.
///
/// Narrow on purpose. A bare 404 with none of these words is a wrong URL —
/// which is a real thing to get wrong on a custom endpoint — and the lane it
/// belongs to may well be fine.
pub fn unsupported_model(status: u16, body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    match status {
        // The Codex backend refuses a slug the account is not entitled to at
        // validation time, before it is a 404: `"<model> is not supported when
        // using Codex with a ChatGPT account"`.
        //
        // Model Studio lists every model it *resells* in `/models` — GLM,
        // MiniMax, MiMo — whether or not this account has subscribed to that
        // product, and asking for an unsubscribed one answers `400 ... product
        // is not activated`. Same fact as a 404 from anyone else: this account
        // cannot serve this id, and it will not be able to for the next buyer
        // either.
        400 => b.contains("not supported when using codex") || b.contains("product is not activated"),
        // And the same vendor's other spelling of the same fact: a model the
        // account is not entitled to answers `403 access_denied` pointing at
        // the Model Studio error index. Matched narrowly on that document,
        // because a bare 403 anywhere else is about the *credential* and is
        // read as such by `refusal_outcome`.
        403 => b.contains("access_denied") && b.contains("model-studio"),
        404 => {
            // Anthropic: `{"type":"not_found_error","message":"model: X"}`.
            // OpenAI: `model_not_found` / "does not exist or you do not have
            // access to it". OpenRouter-shaped resellers: "no endpoints found".
            ["not_found_error", "model_not_found", "does not exist", "no endpoints found"]
                .iter()
                .any(|m| b.contains(m))
        }
        _ => false,
    }
}

/// An account that cannot pay for the call, whatever status its vendor chose
/// to say so in — Anthropic's `400` about extra usage, an aggregator's `400
/// insufficient_user_quota`, OpenRouter's `402`.
///
/// Shape aside, they are all a quota wall: the account cannot serve *anyone*
/// until its owner tops up, and the catch-all below reads a 4xx as the
/// consumer's problem and leaves the lane on the market. On 2026-08-22 that is
/// exactly what happened — one account kept winning matches and answering every
/// one of them with this 400, so buyers saw a dead model rather than a
/// failover. The vocabulary itself lives in the protocol crate, because the
/// gateway re-reads the same body for sellers still on an older client.
pub fn quota_exhausted(status: u16, body: &str) -> bool {
    asale_protocol::is_out_of_credit(status, body)
}

/// How to resolve the upstream bearer token for a provider.
///
/// Both hooks carry the model: availability and failure state are tracked per
/// `(account, model)` lane, so an account that is fine for one model and
/// broken for another can be described accurately (spec §4.5).
pub trait TokenProvider: Send + Sync {
    /// Return the bearer token for a provider (e.g. "claude"), or None.
    fn token_for(&self, provider: &str) -> Option<String>;

    /// The id the provider's upstream knows one of this account's sessions
    /// by. Default: empty, which fingerprints anonymously — acceptable for
    /// providers that never ask for a session, and worse than useless for
    /// Claude, for which a rotating one sticks out more than none at all.
    fn session_for(&self, _account_id: &str) -> Option<String> {
        None
    }

    /// Lease a token for one lane. Pool-backed implementations pick an account
    /// whose lane for `model` is serving (spec §4); the default wraps
    /// `token_for` with no account identity.
    fn acquire(&self, provider: &str, _model: &str) -> Option<LeasedToken> {
        self.token_for(provider).map(|token| LeasedToken { token, ..Default::default() })
    }

    /// Whether an [`acquire`](TokenProvider::acquire) that came back empty did so
    /// only because every account for this lane is at its own concurrency
    /// ceiling — the lane is healthy and a slot frees up when a call in flight
    /// finishes.
    ///
    /// Separated from `acquire` returning `None` because the two want opposite
    /// answers: a lane with no usable account has to be reported, and a lane that
    /// is merely busy has to be waited for. See [`AccountPool::lane_saturated`]
    /// for why the busy case happens at all.
    ///
    /// [`AccountPool::lane_saturated`]: crate::pool::AccountPool::lane_saturated
    fn saturated(&self, _provider: &str, _model: &str) -> bool {
        false
    }

    /// Whether some *other* account on this device could serve this lane right
    /// now — one that is not `except`.
    ///
    /// Asked after a failed attempt has been reported, to decide whether the
    /// task is worth handing to a second account before the buyer is told it
    /// failed. It has to be a question and not just "acquire again and see",
    /// because acquiring takes a concurrency slot that would then have to be
    /// released as either a success or a fault, and it is neither.
    ///
    /// Default: false. A provider with no pool behind it has no second account
    /// to offer, and retrying the one it has against the same credential only
    /// spends the buyer's time.
    fn has_alternate(&self, _provider: &str, _model: &str, _except: &str) -> bool {
        false
    }

    /// Report the outcome of a leased call. Default: no-op.
    fn report(&self, _provider: &str, _account_id: &str, _model: &str, _outcome: TaskOutcome) {}
}

/// How long a relayed call waits for one of this account's concurrency slots
/// before giving up on it.
///
/// Bounded by what the gateway is willing to wait for a first frame
/// (`ASALE_RELAY_IDLE_TIMEOUT_SECS`, five minutes by default), and set far below
/// it: a queue this side is invisible to the buyer except as latency, and thirty
/// seconds of latency is a worse answer than a failover to another seller.
const LEASE_WAIT: Duration = Duration::from_secs(30);

/// How often the wait re-checks. Short enough that a freed slot is picked up
/// promptly, long enough that a saturated account is not spinning a lock.
const LEASE_POLL: Duration = Duration::from_millis(100);

/// Lease a token for this task, waiting out a full lease table rather than
/// handing the work back.
///
/// Work already routed here must not be refused for being early. The market
/// dispatches against the concurrency each *lane* declares, while the ceiling
/// those declarations come from is one budget shared across every model the
/// account sells — so a device selling five models is routinely sent more than
/// it said it could take. Answering that with an error was answering it with a
/// *credential* error (`TOKEN_EXPIRED`), which the gateway reads as a lane that
/// cannot serve: it cools the lane down, doubles the cooldown on the next one
/// and quarantines it after a few, so a burst of ordinary traffic took the lane
/// off the market and kept it off. It is also what made model verification
/// impossible to complete — the probes are the burst, and by the second one the
/// lane they were checking was in cooldown and could no longer be bought from.
async fn lease_for_task(
    tokens: &dyn TokenProvider,
    provider: &str,
    model: &str,
    cancel: &mut tokio::sync::oneshot::Receiver<()>,
) -> Result<LeasedToken, LeaseFailure> {
    let deadline = Instant::now() + LEASE_WAIT;
    loop {
        if let Some(lease) = tokens.acquire(provider, model) {
            return Ok(lease);
        }
        // Nothing to wait for: no account on this device can serve this lane at
        // all, which is a fact about the lane and is reported as one.
        if !tokens.saturated(provider, model) || Instant::now() >= deadline {
            return Err(LeaseFailure::NoAccount);
        }
        tokio::select! {
            _ = tokio::time::sleep(LEASE_POLL) => {}
            // The consumer left while we were queued. Nothing to serve.
            _ = &mut *cancel => return Err(LeaseFailure::Canceled),
        }
    }
}

/// Why [`lease_for_task`] came back without a token.
enum LeaseFailure {
    /// No account on this device can serve this lane — now, or after waiting.
    NoAccount,
    /// The consumer went away while the task was queued for a slot.
    Canceled,
}

/// Sink for completed-task metering records (spec §5.2 step 5). The Tauri layer
/// implements this over the local `provider_records` table for reconciliation.
#[async_trait]
pub trait RecordSink: Send + Sync {
    /// `account_id` is the pool account that served the task — empty when the
    /// failure happened before an account could be leased. Metering is keyed on
    /// it so per-account sell limits and quota estimates stay separate.
    async fn record(&self, task_id: &str, provider: &str, account_id: &str, model: &str, usage: &Usage, status: &str);

    /// Quota headers a provider volunteered on an upstream response.
    ///
    /// Serving a task is the one moment those numbers arrive for free, so they
    /// are handed over here rather than paid for again later by a probe. The
    /// default is a no-op: a sink that only meters need not care.
    async fn observe_quota(&self, _provider: &str, _account_id: &str, _headers: &BTreeMap<String, String>) {}
}

/// The quota headers on an upstream response, as `name -> value`.
///
/// Two families, for two reasons:
///
///   * `x-codex-` — the only reading a ChatGPT bearer can get at all
///     (`/backend-api/codex/usage` answers that credential 403), so serving is
///     where Codex's numbers come from.
///   * `x-ratelimit-` — the conventional OpenAI-style block. xAI is the one
///     provider here whose subscription publishes no usage endpoint of its own
///     (the `rest/rate-limits` call the Grok web app makes is authorised by a
///     web session, not by the CLI's bearer), so whatever it volunteers on a
///     response is the only reading available. Collected opportunistically: an
///     upstream that sends nothing simply yields nothing, and
///     `usage::normalize_ratelimit_headers` throws away the per-minute burst
///     limits that would otherwise be mistaken for a spent subscription.
///
/// Empty for every other provider — Claude, Gemini and Kimi each answer a
/// dedicated endpoint that costs no quota, which is a better reading than a
/// header because it can be taken while the account is idle.
pub fn quota_headers(provider: &str, headers: &reqwest::header::HeaderMap) -> BTreeMap<String, String> {
    let prefix = match Provider::from_str_opt(provider).map(|p| asale_protocol::spec(p).quota) {
        Some(asale_protocol::QuotaSource::Headers(prefix)) => prefix,
        _ => return BTreeMap::new(),
    };
    headers
        .iter()
        .filter(|(k, _)| k.as_str().starts_with(prefix))
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), s.trim().to_string())))
        .collect()
}

/// Execute one relayed request and stream results back via `out`.
///
/// `quota` is built from the key pinned into this build
/// ([`crate::security::pinned_quota_verifier`]) — never from anything the
/// gateway said on this connection. It is not optional: a device with no way to
/// check a grant does not serve at all, so this function is only reachable once
/// a verifier exists.
/// Serve one relayed call.
///
/// `cancel` fires when the gateway sends `task.cancel` — its consumer went away
/// — or when it is dropped, which happens when this session ends. Either way
/// there is nobody left to receive the answer, so the upstream call is dropped
/// where it stands: every token past that point would be this device's own
/// subscription quota spent on output nobody reads, and the gateway bills only
/// what reached it before the buyer left, so it would not even be paid for.
///
/// Cooperative rather than an `abort()` from the caller, because the pool lease
/// has to be reported back to release its concurrency slot (see
/// [`TokenProvider::report`]) and the local metering row is what the seller's own
/// quota accounting is built from. A killed task leaks the first and loses the
/// second.
pub async fn execute(
    http: &reqwest::Client,
    tokens: &dyn TokenProvider,
    req: HttpRequestPayload,
    out: &mpsc::UnboundedSender<Envelope>,
    records: Option<&dyn RecordSink>,
    quota: &QuotaVerifier,
    mut cancel: tokio::sync::oneshot::Receiver<()>,
) {
    let task_id = req.task_id.clone();

    // Anti-over-serve (spec §5.4/§10.5): the server signs
    // {task_id|model|budget|exp}. Verifying it — rather than just checking that
    // some string is present — is what keeps this device from burning its own
    // subscription quota on a task the platform never authorized (a forged or
    // replayed dispatch). `budget_tokens` below is the second, local guard.
    if req.quota_sig.trim().is_empty() {
        send_error(out, &task_id, "QUOTA_SIG_INVALID", "missing quota grant signature", true);
        return;
    }
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Err(e) = quota.verify(&req.task_id, &req.model, req.budget_tokens, req.exp, &req.quota_sig, now) {
            tracing::warn!(task = %task_id, "refusing relayed request: {e}");
            // Retriable: the usual cause is this machine's own quota public
            // key or clock (see the gateway's `is_retriable` floor), and the
            // buyer must not lose a request over one seller's configuration.
            send_error(out, &task_id, "QUOTA_SIG_INVALID", &e.to_string(), true);
            return;
        }
    }

    // The lane this task belongs to. Prefer the model the quota grant names —
    // it is the id the gateway keys its own lane state on, so using it keeps
    // both sides' bookkeeping about the same lane. The request body is the
    // fallback for grants that predate the field.
    let model = if req.model.is_empty() {
        extract_model(&req.upstream.body_b64).unwrap_or_default()
    } else {
        req.model.clone()
    };

    // Resolve + inject the subscription token (only place it is used). The
    // lease picks an account from the pool (spec §4) and must be reported back.
    let provider = req.upstream.provider.clone();
    // How many *other* local accounts one task may fall through to before the
    // buyer is told it failed.
    //
    // The gateway cannot make this hop: a lane is `{device}|{provider}`
    // (`supply_declarations` is keyed on device+provider+model), so seven
    // `custom` accounts on one machine are a single lane to the market — when
    // the failover ladder excludes it for one account's 402, it excludes every
    // one of them and goes looking for a different *device*. Which account
    // served is knowable only here, and so is the fact that six others were
    // standing by.
    const MAX_LOCAL_FAILOVERS: u32 = 2;
    let mut failovers_left = MAX_LOCAL_FAILOVERS;
    // The first attempt's failure, kept while a second account is tried: if
    // nobody else can serve the lane, this is what the buyer is owed.
    let mut held: Option<HeldFailure> = None;
    let (lease, resp, status) = loop {
        let lease = match lease_for_task(tokens, &provider, &model, &mut cancel).await {
            Ok(l) => l,
            // Nobody left to read an error, and no account to blame for one.
            Err(LeaseFailure::Canceled) => {
                if let Some(r) = records {
                    r.record(&task_id, &provider, "", &model, &Usage::default(), "canceled").await;
                }
                return;
            }
            Err(LeaseFailure::NoAccount) => {
                // A retry that found nobody else to hand the task to reports the
                // failure that sent it looking, not "no account" — which is both
                // untrue (there was one; it could not pay) and unactionable.
                match held.take() {
                    Some(f) => f.report(out, records, &task_id, &provider, &model).await,
                    None => {
                        send_error(out, &task_id, "TOKEN_EXPIRED", "no local token for provider", true);
                        if let Some(r) = records {
                            r.record(&task_id, &provider, "", &model, &Usage::default(), "no_token").await;
                        }
                    }
                }
                return;
            }
        };
        let token = lease.token.clone();
        let session_id = match lease.session_id.clone().filter(|s| !s.is_empty()) {
            Some(s) => s,
            None => tokens.session_for(&lease.account_id).unwrap_or_default(),
        };

        let method = reqwest::Method::from_bytes(req.upstream.method.as_bytes()).unwrap_or(reqwest::Method::POST);
        // A `custom` account's endpoint belongs to whoever configured it, so the
        // gateway sends a placeholder and both the real URL and the header the key
        // travels in are assembled here — the one side that knows which account
        // this task was leased against. Everything else uses the URL as built and a
        // bearer: those hosts are the vendors' and are settled at compile time.
        let custom = lease
            .upstream_base
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(|base| (base, lease.upstream_wire.unwrap_or_default()));
        let url = match custom {
            Some((base, wire)) => custom_url(base, wire, &req.upstream.url),
            None => req.upstream.url.clone(),
        };
        let mut builder = http.request(method, &url);
        // A subscription request rebuilds Claude Code's own header identity below,
        // so the gateway's guesses at those names are dropped rather than sent
        // alongside — `reqwest` appends, and two `anthropic-beta` headers is a
        // shape no first-party client ever puts on the wire.
        let claude_identity = is_claude(&provider) && custom.is_none();
        for (k, v) in &req.upstream.headers {
            if let Some(s) = v.as_str() {
                if claude_identity && CLAUDE_IDENTITY_HEADERS.iter().any(|h| k.eq_ignore_ascii_case(h)) {
                    continue;
                }
                builder = builder.header(k, s);
            }
        }
        builder = match custom {
            Some((_, wire)) => authorize_custom(builder, wire, &token, &req.upstream.headers),
            None => builder.header("authorization", format!("Bearer {token}")),
        };
        let mut body = B64.decode(req.upstream.body_b64.as_bytes()).unwrap_or_default();
        // The token we just injected is a Claude Code subscription credential.
        // Anthropic decides "plan vs extra usage" from how much the request reads
        // like the official CLI, so the whole fingerprint is built here, where the
        // credential is known: without this the account answered 400
        // "Third-party apps now draw from your extra usage, not your plan limits",
        // which the gateway misread as a 429 and took the lane off the market
        // (2026-08-23).
        // Whether the fingerprint above actually went on. A body it could not parse
        // is relayed as it arrived — the lane still has Claude Code's own traffic to
        // serve, so this is not worth failing over — but that request is the one
        // shape Anthropic bills to extra usage, and the 400 it earns costs the whole
        // account a cooldown. Recording it is what tells the two cases apart after
        // the fact: without this, "was it cloaked?" can only be guessed from the
        // `system=NNNB` in the shape string.
        let mut cloaked = false;
        if is_claude(&provider) {
            if let Some(patched) = with_claude_code_system(&body, &session_id) {
                body = patched;
                cloaked = true;
            }
            if claude_identity {
                // Read off the *patched* body: the beta set is per-request — tools,
                // thinking and the model all move it — so it can only be assembled
                // once the body is final.
                builder = builder.header("anthropic-beta", claude_code_betas(&body, true));
                for (k, v) in claude_identity_headers(&session_id) {
                    builder = builder.header(k, v);
                }
            }
        }
        // A custom endpoint may know this model by another name — an aggregator
        // lists `anthropic/claude-haiku-4.5` for what the market trades as
        // `claude-haiku-4-5`. The lane was declared, matched and metered under the
        // market id, so only the outgoing body is rewritten, and only here: the
        // account is what decides the spelling, and this is where the account is
        // known.
        if let Some(id) = lease.upstream_model.as_deref().filter(|s| !s.is_empty()) {
            match with_model(&body, id) {
                Some(patched) => body = patched,
                // A body whose model could not be replaced would reach the upstream
                // asking for an id it does not publish, and come back as a 400 that
                // reads like a broken account. Failing here says what is wrong.
                None => {
                    tokens.report(&provider, &lease.account_id, &model, TaskOutcome::ServerError);
                    send_error(out, &task_id, "UPSTREAM_5XX", "could not set the upstream model id", false);
                    if let Some(r) = records {
                        r.record(&task_id, &provider, &lease.account_id, &model, &Usage::default(), "model_rewrite_failed")
                            .await;
                    }
                    return;
                }
            }
        }
        // Kimi Code identifies the calling installation, not just the calling
        // product, and the device id is the one part of that identity the gateway
        // cannot know — it belongs to this publisher. Same reasoning as the Claude
        // block above: applied where the credential is, not where the body is built.
        if provider == "kimi" {
            builder = builder.header("x-msh-device-id", kimi_device_id(&lease.account_id));
        }
        // Codex's upstream authenticates the *pair*: the ChatGPT bearer and the
        // account id it was issued for. With the bearer alone it answers 401 — which
        // reads exactly like a revoked login, so the pool flags the account as
        // needing a fresh sign-in and takes every one of its lanes off the market.
        // The id belongs to the account, so like Kimi's device id it can only be
        // filled in here, next to the token.
        if provider == "codex" {
            let resolved = lease
                .upstream_account_id
                .clone()
                .filter(|s| !s.is_empty())
                // Accounts connected before the id was being recorded have nothing
                // stored, and asking their owner to sign in again to recover a value
                // the token already carries is a poor trade — so read it back off
                // the bearer instead. Same claim the CLI reads out of the id_token,
                // and it is reissued with every refresh, so this keeps working.
                .or_else(|| chatgpt_account_id(&token));
            match resolved {
                Some(acct) => builder = builder.header("chatgpt-account-id", acct),
                // Neither source had it: say so, rather than send a request that can
                // only 401 and then be misread as a revoked credential. Reconnecting
                // the account through asale's own Codex login fills it in.
                None => {
                    tokens.report(&provider, &lease.account_id, &model, TaskOutcome::AuthFailed);
                    send_error(
                        out,
                        &task_id,
                        "TOKEN_EXPIRED",
                        "codex account has no chatgpt-account-id; reconnect the account",
                        // This account on this machine is misconfigured; every
                        // other seller's is not. The gateway hands the buyer on.
                        true,
                    );
                    if let Some(r) = records {
                        r.record(&task_id, &provider, &lease.account_id, &model, &Usage::default(), "no_account_id").await;
                    }
                    return;
                }
            }
        }
        // Fingerprint what we are about to send, before the body is moved into the
        // request. An upstream 4xx names the offending field ("System messages are
        // not allowed") but never which item carried it, and the body itself must
        // not be logged — it is the consumer's prompt. The shape is enough to tell a
        // translator bug from a credential problem, and this is the only place it
        // can be recorded: the gateway builds this body and never sees the
        // rejection; this process sees the rejection and never kept the body.
        let shape = body_shape(&body);
        builder = builder.body(body);

        let send = tokio::select! {
            r = builder.send() => r,
            // Cancelled before the upstream even answered: drop the request future
            // and close the connection, which is what stops the generation.
            _ = &mut cancel => {
                tracing::info!(task = %task_id, "consumer left before the upstream answered; abandoning the call");
                finish_canceled(tokens, records, &provider, &lease.account_id, &model, &task_id, &Usage::default()).await;
                return;
            }
        };
        let resp = match send {
            Ok(r) => r,
            Err(e) => {
                tokens.report(&provider, &lease.account_id, &model, TaskOutcome::ServerError);
                let failure = HeldFailure {
                    code: "UPSTREAM_5XX",
                    message: format!("upstream send: {e}"),
                    detail: String::new(),
                    retriable: true,
                    record_status: "upstream_error".into(),
                    account_id: lease.account_id.clone(),
                };
                // The upstream was never reached, so nothing has been streamed and
                // another account on this device is free to try.
                if failovers_left > 0 && tokens.has_alternate(&provider, &model, &lease.account_id) {
                    failovers_left -= 1;
                    held = Some(failure);
                    continue;
                }
                failure.report(out, records, &task_id, &provider, &model).await;
                return;
            }
        };

        let status = resp.status().as_u16();

        // Bank whatever the provider said about its own quota on the way past —
        // including on the 429 that follows exhaustion, which is the reading the
        // Limits page most wants and the one a later probe could no longer obtain.
        if let Some(sink) = records {
            let observed = quota_headers(&provider, resp.headers());
            if !observed.is_empty() {
                sink.observe_quota(&provider, &lease.account_id, &observed).await;
            }
        }

        if status >= 400 {
            let reset_at = retry_after_reset(&resp);
            // The upstream's own words are the only way to tell a real quota
            // exhaustion from a rejection wearing a 429 (Anthropic masks OAuth
            // policy failures as `rate_limit_error`) — and, for the operator on the
            // other end, the only way to tell "this seller's key is out of credit"
            // from a bare `UPSTREAM_4XX`. Logged here and sent on as the error
            // frame's `detail`, which the gateway records against the task and does
            // not forward to the buyer.
            //
            // Read *before* the pool is told anything, because on a 403 the body is
            // the only thing that says whether the credential or the machine was
            // refused — see [`refusal_outcome`].
            let detail = resp.text().await.unwrap_or_default();
            // Pool feedback: 429 cools the account (honoring Retry-After), 5xx
            // applies the transient cooldown, 401/403 flags the token (spec §4).
            let outcome = match status {
                429 => TaskOutcome::RateLimited { reset_at },
                // Ahead of everything else 4xx: a model the upstream will not serve
                // this account is the one 4xx that says something about the lane
                // rather than about the request, and it takes the lane off the
                // market. Ahead of the credential check in particular, because one
                // vendor says it with a 403 and reading that as a bad credential
                // would flag a key that is working fine.
                s if unsupported_model(s, &detail) => TaskOutcome::Unsupported,
                401 | 403 => refusal_outcome(status, &detail),
                s if s >= 500 => TaskOutcome::ServerError,
                // A 400 that is really "out of credit" — cools the account like the
                // 429 it should have been. See [`quota_exhausted`].
                s if quota_exhausted(s, &detail) => TaskOutcome::QuotaExhausted { reset_at },
                _ => TaskOutcome::Success { tokens_used: 0 },
            };
            // What the gateway acts on is the code, not the status. A quota wall
            // reported as a plain 4xx reads as the consumer's problem: the request
            // dies here instead of moving to another seller, and this lane stays on
            // the market until the client's own re-declaration catches up.
            // `UPSTREAM_RATE_LIMIT` is retriable *and* pulls the supply entry
            // (`Job::abandon`), which is the whole of what an exhausted account
            // needs — see [`quota_exhausted`].
            // A plain 429 is the same news as a quota wall and needs the same code:
            // sent as `UPSTREAM_4XX` it read to the gateway as "the buyer's request
            // was bad", so the lane kept its supply entry, walked back into rotation
            // and 429'd again — each round counted as a plain failure against the
            // device's reputation until it fell below the matching floor — and the
            // buyer was handed `invalid_request_error` for what is a rate limit.
            //
            // Read off the *outcome*, not the status. Deciding by status made
            // every 4xx the buyer's problem: a seller's expired key, a model
            // that account does not publish, a machine the vendor refuses and an
            // empty balance all arrived as a final `UPSTREAM_4XX`, so the
            // gateway never spent a failover attempt on any of them. Over the
            // seven days to 2026-08-25 that was 279 failed orders of which
            // *every one* had exactly one attempt, and 264 of them were the
            // lane's fault rather than the request's.
            //
            // The classification above already knows which is which. The three
            // buckets are: the request is bad (nothing to gain by moving it),
            // the lane is bad (move it, and hold the lane responsible), the
            // upstream is having a moment (move it).
            let (code, retriable) = match &outcome {
                TaskOutcome::QuotaExhausted { .. } | TaskOutcome::RateLimited { .. } => {
                    (protocol::codes::UPSTREAM_RATE_LIMIT, true)
                }
                TaskOutcome::AuthFailed => (protocol::codes::TOKEN_EXPIRED, true),
                TaskOutcome::Unsupported | TaskOutcome::Blocked => (protocol::codes::LANE_UNUSABLE, true),
                TaskOutcome::ServerError => (protocol::codes::UPSTREAM_5XX, true),
                // The catch-all arm of the classification above: a 4xx nothing
                // recognised. It is the one shape that fails identically at
                // every seller, so it is the buyer's to fix and the only one
                // that stops here.
                TaskOutcome::Success { .. } => (protocol::codes::UPSTREAM_4XX, false),
            };
            // Whether the refusal belongs to this *account* rather than to the
            // request. Only the former is worth handing to a second account: a
            // malformed body is refused identically everywhere, and retrying it
            // spends another seller's credit on the same 400. `Blocked` is left out
            // from the other side — it is the machine that was refused, and every
            // account here shares it.
            let account_scoped = matches!(
                &outcome,
                TaskOutcome::QuotaExhausted { .. }
                    | TaskOutcome::RateLimited { .. }
                    | TaskOutcome::AuthFailed
                    | TaskOutcome::Unsupported
                    | TaskOutcome::ServerError
            );
            tokens.report(&provider, &lease.account_id, &model, outcome);
            tracing::warn!(
                task = %task_id, provider = %provider, model = %model, status, cloaked, sent = %shape,
                "upstream rejected: {}", detail.chars().take(400).collect::<String>()
            );
            let failure = HeldFailure {
                code,
                message: format!("upstream {status}"),
                detail,
                retriable,
                record_status: format!("upstream_{status}"),
                account_id: lease.account_id.clone(),
            };
            // `tokens.report` above has already cooled the lane that failed, so the
            // next lease skips it on its own — no exclusion list to thread through.
            if account_scoped && failovers_left > 0 && tokens.has_alternate(&provider, &model, &lease.account_id) {
                failovers_left -= 1;
                held = Some(failure);
                continue;
            }
            failure.report(out, records, &task_id, &provider, &model).await;
            return;
        }
        break (lease, resp, status);
    };

    // A request the consumer did not ask to stream was answered by the upstream
    // with one JSON object, not an SSE stream. Framing it as `stream_chunk`s
    // handed the gateway bytes it then tried to read as SSE lines: no `data:`
    // prefix, so no events, so no text and no usage — the consumer got a 200
    // carrying an empty message and the gateway failed the task for serving
    // nothing (`relay::finalize`), which cost the publisher a sale it had
    // actually made. `http_response` is the frame that exists for this.
    //
    // Which of the two arrived is the upstream's decision, not the consumer's.
    // The gateway forces `stream: true` for Codex, whose ChatGPT backend rejects
    // a buffered Responses call outright, while the relay envelope still carries
    // the consumer's own `stream: false`. Branching on `req.stream` alone
    // therefore buffered an SSE stream into `http_response`, where the gateway
    // read it as JSON, found none, and settled every non-streaming Codex sale as
    // an empty answer worth zero tokens.
    let upstream_is_sse = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim_start().starts_with("text/event-stream"));

    if !req.stream && !upstream_is_sse {
        // The whole generation happens inside this one await for a buffered
        // upstream, so it is the one that matters most to be able to walk away
        // from.
        let read = tokio::select! {
            b = resp.bytes() => b,
            _ = &mut cancel => {
                tracing::info!(task = %task_id, "consumer left while the upstream was answering; abandoning the call");
                finish_canceled(tokens, records, &provider, &lease.account_id, &model, &task_id, &Usage::default()).await;
                return;
            }
        };
        let body = match read {
            Ok(b) => b,
            Err(e) => {
                tokens.report(&provider, &lease.account_id, &model, TaskOutcome::ServerError);
                send_error(out, &task_id, "UPSTREAM_5XX", &format!("body: {e}"), true);
                if let Some(r) = records {
                    r.record(&task_id, &provider, &lease.account_id, &model, &Usage::default(), "upstream_error").await;
                }
                return;
            }
        };
        let usage = usage_from_body(&body);
        let _ = out.send(Envelope::with_id(
            &task_id,
            protocol::T_HTTP_RESPONSE,
            json!({
                "task_id": task_id,
                "status": status,
                "body_b64": B64.encode(&body),
                "usage": {
                    "input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens,
                    "cache_read_tokens": usage.cache_read_tokens, "cache_write_tokens": usage.cache_write_tokens
                }
            }),
        ));
        tokens.report(
            &provider,
            &lease.account_id,
            &model,
            TaskOutcome::Success { tokens_used: usage.quota_tokens().max(0) as u64 },
        );
        if let Some(r) = records {
            r.record(&task_id, &provider, &lease.account_id, &model, &usage, "ok").await;
        }
        return;
    }

    // stream_start
    let _ = out.send(Envelope::with_id(
        &task_id,
        protocol::T_STREAM_START,
        json!({"task_id": task_id, "status": status, "headers": {}}),
    ));

    // Stream body chunks; parse usage from SSE where possible. The scanner holds
    // a line that a chunk boundary cut in half — without it the Responses
    // dialect's usage frame is lost and the sale settles as zero tokens.
    let mut stream = resp.bytes_stream();
    let mut seq: u64 = 0;
    let mut scan = UsageScanner::new();
    let mut budget_hit = false;

    loop {
        let next = tokio::select! {
            c = stream.next() => c,
            // Stop mid-answer. What was already scanned is reported to the pool
            // below, so the quota this call really did spend still decays.
            _ = &mut cancel => {
                let usage = scan.flush();
                tracing::info!(
                    task = %task_id, out_tokens = usage.output_tokens,
                    "consumer left mid-stream; abandoning the rest of the answer"
                );
                finish_canceled(tokens, records, &provider, &lease.account_id, &model, &task_id, &usage).await;
                return;
            }
        };
        let Some(chunk) = next else { break };
        let bytes = match chunk {
            Ok(b) => b,
            Err(e) => {
                tokens.report(&provider, &lease.account_id, &model, TaskOutcome::ServerError);
                send_error(out, &task_id, "UPSTREAM_5XX", &format!("stream: {e}"), true);
                if let Some(r) = records {
                    let usage = scan.flush();
                    r.record(&task_id, &provider, &lease.account_id, &model, &usage, "stream_error").await;
                }
                return;
            }
        };
        scan.push(&bytes);
        // Budget guard: interrupt if output exceeds the granted budget.
        //
        // What it can *do* about it depends on when it finds out, and that is
        // the upstream's choice rather than ours — see `UsageScanner::completed`.
        // A cap the upstream could enforce itself never gets here at all; the
        // gateway forwards it as `max_tokens` / `max_output_tokens` and the
        // vendor stops on its own. The exception is the ChatGPT Codex backend,
        // which answers `400 {"detail":"Unsupported parameter:
        // max_output_tokens"}` to the field and then reports its usage only at
        // the end — so on that one lane this guard is always retroactive, and
        // the whole of its effect is the finish reason below.
        if scan.usage().output_tokens > req.budget_tokens && req.budget_tokens > 0 {
            budget_hit = true;
        }
        let _ = out.send(Envelope::with_id(
            &task_id,
            protocol::T_STREAM_CHUNK,
            json!({"task_id": task_id, "seq": seq, "data_b64": B64.encode(&bytes)}),
        ));
        seq += 1;
        if budget_hit {
            // Only an error if there was something left to cut off. A stream
            // that has already signed off delivered a whole answer, and ending
            // it with an error frame made the gateway record a settled call as
            // `interrupted`, put a failure on the seller's lane, and hand the
            // buyer a complete reply with a fault stapled to the end of it.
            // The bill is bounded either way — the gateway caps usage at the
            // budget it signed — so the frame bought nothing.
            if !scan.completed() {
                send_error(out, &task_id, "BUDGET_EXCEEDED", "output exceeded budget", false);
            }
            break;
        }
    }

    // stream_end with the best usage we have — including a final frame that
    // arrived without a trailing newline.
    let usage = scan.flush();
    let _ = out.send(Envelope::with_id(
        &task_id,
        protocol::T_STREAM_END,
        json!({
            "task_id": task_id,
            "finish_reason": if budget_hit { "budget" } else { "end_turn" },
            "usage": {
                "input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens,
                "cache_read_tokens": usage.cache_read_tokens, "cache_write_tokens": usage.cache_write_tokens
            }
        }),
    ));

    // Release the pool lease with the measured usage (quota decay, spec §4).
    tokens.report(
        &provider,
        &lease.account_id,
        &model,
        TaskOutcome::Success { tokens_used: usage.quota_tokens().max(0) as u64 },
    );

    // Local metering record for reconciliation (spec §5.2 step 5, §8).
    if let Some(r) = records {
        let status_label = if budget_hit { "budget" } else { "ok" };
        r.record(&task_id, &provider, &lease.account_id, &model, &usage, status_label).await;
    }
}

/// Close the books on a call the gateway told us to abandon.
///
/// No frame goes back: the gateway closed the task's channel the moment it gave
/// up on the consumer, so a `stream_end` would be routed to nothing. What does
/// have to happen is local — release the pool lease (its concurrency slot is held
/// until an outcome is reported) with the quota this call really did spend, and
/// keep the metering row, because the seller's own usage and sell-limit
/// accounting is built from those rows and this call consumed just as much of
/// the subscription as one that finished.
async fn finish_canceled(
    tokens: &dyn TokenProvider,
    records: Option<&dyn RecordSink>,
    provider: &str,
    account_id: &str,
    model: &str,
    task_id: &str,
    usage: &Usage,
) {
    tokens.report(
        provider,
        account_id,
        model,
        TaskOutcome::Success { tokens_used: usage.quota_tokens().max(0) as u64 },
    );
    if let Some(r) = records {
        r.record(task_id, provider, account_id, model, usage, "canceled").await;
    }
}

/// Whether a relayed request will be served with a Claude subscription token.
fn is_claude(provider: &str) -> bool {
    asale_protocol::ids::is_claude_family(provider)
}

/// A stable id for Kimi Code's `X-Msh-Device-Id` header, derived from the
/// account the request is being served with.
///
/// It has to be *stable*: a value that changed per request would make one
/// publisher look like a fleet of machines sharing a subscription, which is the
/// pattern a vendor watches for. Deriving it from the account id rather than
/// from machine state also keeps it deterministic on a device with no writable
/// identity file, and keeps two accounts on one machine distinct — which is
/// what they are as far as Moonshot is concerned.
///
/// Shaped like a UUID because that is what the vendor CLI sends.
#[cfg(test)]
pub(crate) fn kimi_device_id_for_test(account_id: &str) -> String {
    kimi_device_id(account_id)
}

/// Read the ChatGPT account id out of a Codex bearer's own claims.
///
/// A ChatGPT OAuth access token is a JWT, and the `https://api.openai.com/auth`
/// claim inside it names the account it was issued for — the same value the
/// Codex CLI keeps in `auth.json`. Deriving it here is what lets an account that
/// was connected before asale recorded the id keep selling without its owner
/// having to sign in again.
///
/// Best-effort by design: a token that is not a JWT, or a JWT without the claim,
/// simply yields `None` and the caller falls back to saying what is missing.
pub fn chatgpt_account_id(bearer: &str) -> Option<String> {
    crate::cli_import::jwt_claims(bearer)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn kimi_device_id(account_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(format!("asale-kimi-device:{account_id}").as_bytes());
    let h: String = d[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Build the full Claude Code fingerprint into an upstream request body.
///
/// Anthropic draws "plan limits" only for requests it recognises as Claude
/// Code; anything else taps "extra usage". A bare system preamble is not
/// enough — the checks are layered, modelled on CLIProxyAPI's cloaking
/// (`claude_executor_cloaking.go`):
///
///  * a first system block carrying `x-anthropic-billing-header` with the
///    same version attribution the CLI puts there (`cc_version=2.1.220.xxx`),
///  * a second block with the `You are Claude Code…` preamble and the
///    ephemeral cache breakpoint the CLI sets,
///  * `metadata.user_id` as the CLI's own JSON blob
///    (`{device_id, account_uuid, session_id}`) — one stable session per account
///    rather than a one-request client (empty session id for an account that
///    cannot be named yields metadata that is simply skipped),
///  * a `# currentDate` reminder at the head of the first user turn, like the
///    CLI itself injects,
///  * the caller's own system prompt, relocated into the conversation with
///    the provider's own authority rather than stamped into the system slot
///    where a third-party request would dump it.
///
/// Fully compliant bodies are returned unchanged. Anything that is not JSON
/// object-shaped returns `None`, so the caller falls back to what the gateway
/// built on its own.
pub fn with_claude_code_system(body: &[u8], session_id: &str) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    // Take the fingerprint's walk of the conversation opener before borrowing
    // obj, so the two do not overlap against the same root.
    let fingerprint_hash = fingerprint_hash(first_user_text(&v));
    let obj = v.as_object_mut()?;

    // Snapshot the caller's system prompt before it is replaced with the
    // fingerprint blocks.
    let caller_system: Vec<String> = match obj.get("system") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    let caller_system: Vec<String> = caller_system
        .into_iter()
        // Our own billing header is ours to re-emit, so the whole block goes.
        .filter(|t| !t.starts_with(BILLING_PREFIX))
        // The preamble is *not*: Claude Code's own traffic carries it at the
        // head of the same block as its real system prompt, so dropping the
        // block on the prefix threw the caller's actual instructions — tools,
        // environment, CLAUDE.md — away and sent the model a bare preamble.
        // Only the preamble is re-emitted below; the tail is still the
        // caller's.
        .map(|t| match t.strip_prefix(CLAUDE_CODE_SYSTEM) {
            Some(rest) => rest.trim_start().to_string(),
            None => t,
        })
        .filter(|t| !t.trim().is_empty())
        .collect();

    // metadata.user_id, one stable session per serving account — but only when
    // the caller has none. A local Claude Code request arrives with the real
    // one the CLI minted, and replacing a genuine id with a synthetic one is
    // the opposite of what this block is for. The gateway drops `metadata`, so
    // on the sale path there is never one to keep.
    if !session_id.is_empty() {
        if let Some(m) = obj.entry("metadata").or_insert_with(|| json!({})).as_object_mut() {
            let keep = m.get("user_id").and_then(|u| u.as_str()).is_some_and(is_claude_code_user_id);
            if !keep {
                m.insert("user_id".into(), json!(claude_code_user_id(session_id)));
            }
        }
    }

    // The `# currentDate` reminder that the CLI stamps into every request,
    // kept out of the first user message's tool-result lead-in.
    inject_current_date(&mut v);

    // system := [billing header, Claude Code preamble, the caller's own blocks].
    //
    // Leading with a Claude Code marker is the *whole* of what buys plan usage
    // rather than extra usage — measured against a live max20 subscription on
    // 2026-08-23, bisecting an 83KB / 10-tool body one property at a time:
    // dropping the metadata block, the billing line, the preamble, the
    // `# currentDate` reminder or every `cache_control` breakpoint each still
    // answered 200, and so did the same body with none of the CLI's headers.
    // The one shape that was refused, twice and reproducibly, is a `system`
    // whose first block is the caller's own prompt. So the caller's prompt does
    // not have to leave the system slot at all; it only has to come second.
    //
    // Which retires a mid-conversation `role: system` turn, the beta flag that
    // unlocked it, and the versioned list of models that refuse it — machinery
    // built to clear a wall that turns out not to look there, at the cost of
    // demoting the caller's instructions from system authority to a chat turn.
    let billing = format!(
        "{prefix} cc_version={version}.{hash}; cc_entrypoint=cli;",
        prefix = BILLING_PREFIX,
        version = CLAUDE_CODE_VERSION,
        hash = fingerprint_hash
    );
    let mut system = vec![
        json!({"type": "text", "text": billing}),
        json!({"type": "text", "text": CLAUDE_CODE_SYSTEM}),
    ];
    system.extend(caller_system.into_iter().map(|t| json!({"type": "text", "text": t})));
    v.as_object_mut().unwrap().insert("system".to_string(), json!(system));
    // One ephemeral breakpoint, on the last block, so the caller's prompt is
    // inside the cached prefix rather than after it — but only if the relayed
    // body has not already spent the four Anthropic allows.
    if spent_breakpoints(&v) < MAX_BREAKPOINTS {
        if let Some(last) =
            v.get_mut("system").and_then(|s| s.as_array_mut()).and_then(|a| a.last_mut()).and_then(|b| b.as_object_mut())
        {
            last.insert("cache_control".into(), json!({"type": "ephemeral"}));
        }
    }

    serde_json::to_vec(&v).ok()
}

/// Cache breakpoints Anthropic accepts on one request. A fifth is a `400`, and
/// the whole turn with it.
const MAX_BREAKPOINTS: usize = 4;

/// How many breakpoints a body already spends outside the system slot.
///
/// The system slot is excluded because [`with_claude_code_system`] rebuilds it
/// and re-emits the caller's blocks as plain text, so whatever it carried is
/// gone by the time this matters. What is left — the caller's tools and
/// messages — is not ours to drop, so it is the two breakpoints this file adds
/// that give way: a missed one re-reads a prefix, a fifth one costs the turn.
fn spent_breakpoints(v: &serde_json::Value) -> usize {
    fn walk(v: &serde_json::Value) -> usize {
        match v {
            serde_json::Value::Object(o) => {
                usize::from(o.contains_key("cache_control")) + o.values().map(walk).sum::<usize>()
            }
            serde_json::Value::Array(a) => a.iter().map(walk).sum(),
            _ => 0,
        }
    }
    match v.as_object() {
        Some(o) => o.iter().filter(|(k, _)| k.as_str() != "system").map(|(_, x)| walk(x)).sum(),
        None => 0,
    }
}

/// The line Claude Code prepends to its system prompt.
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";
/// The version the fingerprint claims. Bumped together with the UA the
/// gateway stamps for this provider.
const CLAUDE_CODE_VERSION: &str = "2.1.220";
/// The fingerprint hash Claude Code puts behind the billing version, derived
/// from the latest user message's text so it tracks the request. Mirrors
/// CLIProxyAPI's `computeFingerprint`.
fn fingerprint_hash(message_text: String) -> String {
    let chars: Vec<char> = message_text.chars().collect();
    let picked: String = [4usize, 7, 20].iter().map(|&i| chars.get(i).copied().unwrap_or('0')).collect();
    let input = format!("{FINGERPRINT_SALT}{picked}{CLAUDE_CODE_VERSION}");
    let hash = format!("{:x}", sha2::Sha256::digest(input.as_bytes()));
    hash[..3].to_string()
}

/// The salt Claude Code mixes into its build fingerprint before hashing.
/// Mirrors CLIProxyAPI's `fingerprintSalt`.
const FINGERPRINT_SALT: &str = "59cf53e54c78";

/// The conversation opener's raw text, without wrapping wrappers. Feeds the
/// billing fingerprint.
///
/// CLIProxyAPI's `claudeBillingFingerprintMessageText` hashes the *latest* user
/// turn instead, and this mirrored it until 2026-08-23. The billing block is
/// `system[0]` — the head of everything a `cache_control` breakpoint can cache —
/// so a hash that moves with the latest turn rewrites the cached prefix on every
/// request: over 482 production Claude-lane relays, 204 wrote a cache and *zero*
/// ever read one back, each turn re-billing the whole prefix at the 1.25x write
/// rate. Hashing the first user turn keeps the block's shape and its
/// per-conversation variety while holding it byte-stable for the length of a
/// conversation, which is what the cache needs.
fn first_user_text(v: &serde_json::Value) -> String {
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else {
        return String::new();
    };
    // The *first* user turn that actually carries text, and within it the last
    // text part — a leading tool-result-only turn falls through to the next one
    // rather than pinning the whole conversation to the empty string.
    for msg in msgs {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let candidate = match msg.get("content") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(parts)) => parts
                .iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .next_back()
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        };
        if !candidate.is_empty() {
            return candidate;
        }
    }
    String::new()
}

/// A device id for the metadata block, stable per daemon. 64 hex characters,
/// which is the shape Claude Code writes and the only one Anthropic's own
/// clients ever send.
fn claude_device_id() -> String {
    use std::sync::OnceLock;
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
    })
    .clone()
}

/// `metadata.user_id` in the shape Claude Code 2.1.78 and newer send: a JSON
/// *string* holding `{device_id, account_uuid, session_id}`. Mirrors
/// CLIProxyAPI's `generateFakeUserIDWithSessionID`.
///
/// The old `user_<device>_session_<session>` spelling this replaced was retired
/// by the CLI years before the 2.1.220 the billing header claims, so sending it
/// alongside that version was itself a third-party tell.
fn claude_code_user_id(session_id: &str) -> String {
    json!({
        "device_id": claude_device_id(),
        "account_uuid": "",
        "session_id": session_uuid(session_id),
    })
    .to_string()
}

/// The session id as a uuid, which is the only shape Claude Code's
/// `metadata.user_id` and `x-claude-code-session-id` carry. An id this side
/// minted in some other spelling is hashed into one rather than replaced by a
/// fresh one each call — the whole value of the id is that it does not move.
fn session_uuid(session_id: &str) -> String {
    if let Ok(u) = uuid::Uuid::parse_str(session_id) {
        return u.to_string();
    }
    let d = sha2::Sha256::digest(format!("asale-claude-session:{session_id}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&d[..16]);
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// Whether a caller's own `metadata.user_id` is one Claude Code would have
/// written — the only kind worth keeping. Mirrors CLIProxyAPI's `isValidUserID`.
fn is_claude_code_user_id(user_id: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(user_id) else { return false };
    let device = v.get("device_id").and_then(|d| d.as_str()).unwrap_or_default();
    if device.len() != 64 || !device.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return false;
    }
    if uuid::Uuid::parse_str(v.get("session_id").and_then(|s| s.as_str()).unwrap_or_default()).is_err() {
        return false;
    }
    match v.get("account_uuid").and_then(|a| a.as_str()).unwrap_or_default() {
        "" => true,
        a => uuid::Uuid::parse_str(a).is_ok(),
    }
}

/// Index of the first message whose `role` is `user`. A body with no user
/// turn is one the fingerprint leaves alone — Anthropic cannot read a
/// membership in a body without one regardless.
fn first_user_index(v: &serde_json::Value) -> Result<usize, ()> {
    let msgs = v.get("messages").and_then(|m| m.as_array());
    msgs.and_then(|msgs| {
        msgs.iter()
            .position(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    })
    .ok_or(())
}

/// Stamp the current-date reminder at the head of the first user turn.
///
// ponytail: UTC, not the caller's timezone. The CLI stamps a local date, so
// this is off by a day for part of each day east/west of Greenwich; making it
// local needs a tz database, which is a dependency for a cosmetic field.
// Upgrade path: `chrono`/`jiff` if the date ever has to match exactly.
fn inject_current_date(v: &mut serde_json::Value) {
    let Ok(index) = first_user_index(v) else { return };
    // Whether there is room for the breakpoint below. A body that already
    // spends all four keeps its own; adding a fifth is a `400` on the turn.
    let room = spent_breakpoints(v) < MAX_BREAKPOINTS;
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    let Some(msg) = msgs.get_mut(index) else { return };
    let now = std::time::SystemTime::now();
    let secs = now.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let date = current_date_string(secs);
    let reminder_text = current_date_reminder(&date);

    match msg.get_mut("content") {
        None => {}
        Some(c @ serde_json::Value::String(_)) => {
            // A bare string becomes the array form the CLI itself sends,
            // with the date marker ahead of the user's own block.
            let body = c.as_str().unwrap().to_string();
            // A prior marker can be appended to the same block in the
            // unusual shape — drop the duplicate rather than stack it.
            let body = strip_prior_date_marker(&body);
            let mut block = json!({"type": "text", "text": body});
            if room {
                block.as_object_mut().unwrap().insert("cache_control".into(), json!({"type": "ephemeral"}));
            }
            *c = json!([{"type": "text", "text": reminder_text.clone()}, block]);
        }
        Some(serde_json::Value::Array(parts)) => {
            // Strip any prior date markers so re-cloaking a body does not
            // stack two reminders.
            let mut kept: Vec<serde_json::Value> = parts
                .iter()
                .filter(|p| !p.get("text").and_then(|t| t.as_str()).map(is_current_date_marker).unwrap_or(false))
                .cloned()
                .collect();
            // Claude Code puts a cache breakpoint on the user's first real text
            // block — the one that is not another `<system-reminder>` wrapper —
            // so the prefix ahead of it is read back instead of re-billed every
            // turn. Without it a seller pays full input price on every request
            // of a long conversation.
            if room {
                if let Some(first_text) = kept.iter_mut().find(|p| {
                    p.get("type").and_then(|t| t.as_str()) == Some("text")
                        && !p.get("text").and_then(|t| t.as_str()).unwrap_or_default().starts_with("<system-reminder>")
                }) {
                    if let Some(o) = first_text.as_object_mut() {
                        o.entry("cache_control").or_insert(json!({"type": "ephemeral"}));
                    }
                }
            }
            let insert_at = kept
                .iter()
                .position(|p| p.get("type").and_then(|t| t.as_str()) != Some("tool_result"))
                .unwrap_or(kept.len());
            *parts = kept;
            parts.splice(
                usize::min(insert_at, parts.len())..usize::min(insert_at, parts.len()),
                vec![json!({"type": "text", "text": reminder_text})],
            );
        }
        Some(_) => {}
    }
}

/// The `# currentDate` block Claude Code stamps into the first user turn,
/// byte-for-byte — the trailing IMPORTANT paragraph and the two closing
/// newlines included. Mirrors CLIProxyAPI's `claudeCodeCurrentDateReminder`;
/// a shortened copy is a different string, which is the whole point of it.
fn current_date_reminder(date: &str) -> String {
    format!(
        "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is {date}.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n\n"
    )
}

fn is_current_date_marker(t: &str) -> bool {
    t.starts_with("<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is ")
}

/// Drop any prior date marker from a string that is about to be re-cloaked,
/// so a second pass does not stack two reminders onto the one block.
fn strip_prior_date_marker(s: &str) -> String {
    match s.find("</system-reminder>") {
        // Only strip when the string actually opens with the marker shape.
        Some(end) if is_current_date_marker(s) => s[end + "</system-reminder>".len()..].trim_start_matches('\n').to_string(),
        _ => s.to_string(),
    }
}

/// The date baked into the reminder, as UTC civil time. Off-by-one-day at the
/// boundaries against a caller in another timezone, which is fine because the
/// date is only a fingerprint.
fn current_date_string(unix_secs: u64) -> String {
    // std has no civil-from-unix converter, and the format needs no library.
    // Do a plain Gregorian rolling: a year is either 365 or 366 days.
    let days = unix_secs / 86_400;
    let mut year = 1970u64;
    let mut remainder = days;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let done = if leap { 366 } else { 365 };
        if remainder < done {
            break;
        }
        remainder -= done;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for len in month_days {
        if remainder < len {
            break;
        }
        remainder -= len;
        month += 1;
    }
    let day = remainder + 1;
    format!("{year}-{month:02}-{day:02}")
}

/// Parse a Retry-After header into an absolute unix-seconds reset, if present.
fn retry_after_reset(resp: &reqwest::Response) -> Option<i64> {
    let secs: i64 = resp.headers().get("retry-after")?.to_str().ok()?.trim().parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some(now + secs.max(0))
}

/// Extract a `model` field from a base64 JSON request body, if present.
pub fn extract_model(body_b64: &str) -> Option<String> {
    let bytes = B64.decode(body_b64.as_bytes()).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("model").and_then(|m| m.as_str()).map(String::from)
}

/// A privacy-safe fingerprint of an upstream request body.
///
/// Records only *shape*: which top-level keys the body carries, the
/// `type:role` sequence of its `input`/`messages` items, how many tools, and the
/// size — never any text. That is deliberately the one thing missing when an
/// upstream 4xx has to be diagnosed from a publisher's log: the rejection names
/// a field it dislikes, and the request that carried it is the buyer's prompt,
/// which this process must not write to disk. A role sequence answers the
/// question the message text cannot — whether the gateway's translator emitted
/// an item this upstream refuses to accept at all.
fn body_shape(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return format!("non-json {}B", body.len());
    };
    let Some(o) = v.as_object() else {
        return format!("json non-object {}B", body.len());
    };
    let mut keys: Vec<&str> = o.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut parts = vec![format!("{}B keys=[{}]", body.len(), keys.join(","))];
    // The two spellings of "the instruction block", by dialect. Length only: the
    // interesting failures are an empty one and a missing one.
    for field in ["instructions", "system"] {
        if let Some(val) = o.get(field) {
            let n = val.as_str().map(str::len).unwrap_or_else(|| val.to_string().len());
            parts.push(format!("{field}={n}B"));
        }
    }
    for field in ["input", "messages"] {
        if let Some(items) = o.get(field).and_then(|v| v.as_array()) {
            let seq: Vec<String> = items
                .iter()
                .map(|it| {
                    let t = it.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let r = it.get("role").and_then(|v| v.as_str()).unwrap_or("");
                    match (t, r) {
                        ("", "") => "?".to_string(),
                        ("", r) => r.to_string(),
                        (t, "") => t.to_string(),
                        (t, r) => format!("{t}:{r}"),
                    }
                })
                .collect();
            parts.push(format!("{field}=[{}]", seq.join(" ")));
        }
    }
    if let Some(n) = o.get("tools").and_then(|v| v.as_array()).map(Vec::len) {
        parts.push(format!("tools={n}"));
    }
    parts.join(" ")
}

/// Reads usage out of an SSE stream as its transport chunks arrive.
///
/// [`accumulate_usage`] can only see the bytes handed to it, and a chunk
/// boundary falls wherever the network puts it — mid-line as often as not. The
/// Claude dialect got away with being fed raw chunks because its usage frame
/// (`message_delta`) is around a hundred bytes and so is effectively never
/// split. The Responses API keeps usage in `response.completed`, whose payload
/// carries the whole response object — 1.4 KB even for a two-word answer — and
/// is therefore split almost every time. Both halves parse as nothing, so every
/// Codex sale reported zero tokens; the gateway reads zero usage as "nothing was
/// served" (`relay::finalize`) and turns a perfectly good sale into a failed
/// task, an unpaid publisher and a penalized lane. Ten of those and the
/// publisher's reputation is under the matching floor and it is off the market.
///
/// The fix is the one thing a per-chunk call cannot have: the tail of the
/// previous chunk. Feed every chunk to [`push`](Self::push) and take the total
/// from [`flush`](Self::flush) when the stream ends.
#[derive(Debug, Default)]
pub struct UsageScanner {
    usage: Usage,
    /// Bytes after the last newline seen — an SSE line still being delivered.
    partial: Vec<u8>,
    /// Whether the stream has announced its own end.
    completed: bool,
}

/// Cap on the held fragment. SSE is newline-framed, so a line this long means
/// the peer is not speaking SSE at all; dropping the fragment keeps a long-lived
/// stream from growing a buffer without bound.
const MAX_PARTIAL_LINE: usize = 1 << 20;

impl UsageScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one transport chunk: every complete line in it is parsed, and the
    /// trailing fragment is held until the rest of it arrives.
    pub fn push(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        match self.partial.iter().rposition(|b| *b == b'\n') {
            Some(cut) => {
                let complete: Vec<u8> = self.partial.drain(..=cut).collect();
                accumulate_usage(&mut self.usage, &complete);
                self.completed |= stream_ended(&complete);
            }
            // No line has ended yet. Keep waiting — unless what we are holding
            // has stopped being plausibly one SSE line.
            None if self.partial.len() > MAX_PARTIAL_LINE => self.partial.clear(),
            None => {}
        }
    }

    /// Usage seen so far. The budget guard reads this mid-stream, so it must not
    /// consume the scanner.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Whether the upstream has signed off on this stream.
    ///
    /// The budget guard needs the difference between "the model is still
    /// writing and has already passed the cap" and "the cap was only exceeded
    /// by the usage frame the stream ends with". Only the first is a runaway
    /// worth cutting into; the second is an answer that arrived complete, and
    /// treating it as an error told the buyer their finished reply had failed.
    ///
    /// The two cases are not a matter of taste about which dialect reports
    /// usage when — they are exactly that. Anthropic counts output as it goes,
    /// so a runaway is caught while there is still something to stop. The
    /// Responses dialect reports usage once, in `response.completed`, so the
    /// guard cannot fire there until the generation is already paid for.
    pub fn completed(&self) -> bool {
        self.completed
    }

    /// Parse whatever fragment is still held and return the total. For a stream
    /// whose last line carries no trailing newline; call it once the stream is
    /// over (or on the error path out of it).
    pub fn flush(&mut self) -> Usage {
        if !self.partial.is_empty() {
            let tail = std::mem::take(&mut self.partial);
            accumulate_usage(&mut self.usage, &tail);
        }
        self.usage
    }
}

/// Extract usage from provider SSE bodies (Claude/OpenAI/Responses/Gemini
/// shapes). Takes a whole body — for a chunked stream use [`UsageScanner`],
/// which carries a split line across the boundary.
pub fn accumulate_usage(usage: &mut Usage, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    for line in text.split('\n') {
        let line = line.trim();
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
        };
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
            merge_usage(usage, &v);
        }
    }
}

/// Whether an SSE fragment carries a dialect's own "that was all" marker.
///
/// Each vendor spells it differently and Gemini does not spell it at all — its
/// stream simply stops — so a `false` here means "no proof the stream is over",
/// which is the safe reading for the one caller: [`UsageScanner::completed`],
/// whose `true` suppresses an error.
fn stream_ended(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    text.split('\n').any(|line| {
        let Some(payload) = line.trim().strip_prefix("data:") else { return false };
        let payload = payload.trim();
        // OpenAI chat completions.
        if payload == "[DONE]" {
            return true;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else { return false };
        matches!(
            v.get("type").and_then(|t| t.as_str()),
            // Responses: the terminal event, whichever way it ended. `incomplete`
            // is the upstream's own truncation (its cap, not ours) and `failed`
            // is an upstream error — both mean nothing further is coming.
            Some("response.completed" | "response.incomplete" | "response.failed")
                // Anthropic.
                | Some("message_stop")
        )
    })
}

/// Usage from a non-streaming response body: the same dialect shapes, without
/// the SSE framing around them.
pub fn usage_from_body(bytes: &[u8]) -> Usage {
    let mut usage = Usage::default();
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
        merge_usage(&mut usage, &v);
    }
    usage
}

/// Read whichever dialect's usage shape this JSON object carries, folding it
/// into the running total.
///
/// The parsing itself is `token_meter::Usage` — the same code asale-server bills
/// from, so a lane's local record and its settled invoice cannot disagree about
/// what the provider said. Two things it gets right that a per-dialect reader
/// here kept getting wrong:
///
///   * **Whether the prompt count already contains the cached part.** Anthropic
///     reports the cache counts *beside* `input_tokens`, which then holds only
///     the uncached remainder. OpenAI (chat and Responses alike) and Gemini fold
///     them *into* the prompt total, so the cached share has to come back out or
///     the same tokens are billed twice — once at the full prompt rate and again
///     as a cache read, a ~10x gap.
///   * **That Gemini bills its thinking outside `candidatesTokenCount`.**
///     `thoughtsTokenCount` is a sibling field charged at the output rate, and
///     reading only candidates under-reported every reasoning turn — sometimes
///     by most of it.
fn merge_usage(usage: &mut Usage, v: &serde_json::Value) {
    let mut m = token_meter::Usage {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_tokens,
        cache_write: usage.cache_write_tokens,
        reasoning: 0,
    };
    m.merge_response(v);
    usage.input_tokens = m.input;
    usage.output_tokens = m.output;
    usage.cache_read_tokens = m.cache_read;
    usage.cache_write_tokens = m.cache_write;
}

/// A failed attempt held back while another local account is tried.
///
/// Reporting is deferred rather than duplicated because the buyer must hear
/// exactly one error, and which one depends on something not yet known when the
/// failure happens: whether this device has another account that can serve the
/// lane. See the failover loop in [`execute`].
struct HeldFailure {
    code: &'static str,
    message: String,
    detail: String,
    retriable: bool,
    record_status: String,
    account_id: String,
}

impl HeldFailure {
    async fn report(
        self,
        out: &mpsc::UnboundedSender<Envelope>,
        records: Option<&dyn RecordSink>,
        task_id: &str,
        provider: &str,
        model: &str,
    ) {
        send_error_detail(out, task_id, self.code, &self.message, &self.detail, self.retriable);
        if let Some(r) = records {
            r.record(task_id, provider, &self.account_id, model, &Usage::default(), &self.record_status).await;
        }
    }
}

fn send_error(out: &mpsc::UnboundedSender<Envelope>, task_id: &str, code: &str, message: &str, retriable: bool) {
    send_error_detail(out, task_id, code, message, "", retriable);
}

/// The same, plus the upstream's own body.
///
/// `message` is what the buyer is shown, so it stays the short summary it has
/// always been. `detail` is the provider's verbatim answer, which is the only
/// thing that separates "this lane failed" from "this lane's account is out of
/// credit" — the gateway writes it to the task row for the operator console and
/// never forwards it. Capped here rather than there: a provider that answers an
/// error with a megabyte of HTML should not be relayed a megabyte of HTML.
fn send_error_detail(
    out: &mpsc::UnboundedSender<Envelope>,
    task_id: &str,
    code: &str,
    message: &str,
    detail: &str,
    retriable: bool,
) {
    let detail: String = detail.trim().chars().take(1000).collect();
    let _ = out.send(Envelope::with_id(
        task_id,
        protocol::T_ERROR,
        json!({
            "id": task_id, "task_id": task_id, "code": code, "message": message,
            "detail": detail, "retriable": retriable,
        }),
    ));
}

#[cfg(test)]
mod tests {
    /// The buyer's half and the operator's half of an upstream rejection travel
    /// in different fields, and the operator's half is bounded.
    #[test]
    fn an_upstream_body_rides_along_as_detail_and_is_capped() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let body = format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(4000));
        super::send_error_detail(&tx, "t1", "UPSTREAM_4XX", "upstream 402", &body, false);
        let env = rx.try_recv().unwrap();
        assert_eq!(env.payload["message"], "upstream 402", "this is what the buyer is shown");
        let detail = env.payload["detail"].as_str().unwrap();
        assert_eq!(detail.chars().count(), 1000, "a megabyte of HTML is not relayed");
        assert!(detail.starts_with(r#"{"error""#));
        // The plain sender keeps its shape: nothing to say, nothing sent.
        super::send_error(&tx, "t2", "TOKEN_EXPIRED", "no local token", true);
        assert_eq!(rx.try_recv().unwrap().payload["detail"], "");
    }

    #[test]
    fn a_geo_refusal_is_not_a_dead_login() {
        use super::{refusal_outcome, TaskOutcome};
        // Verbatim from a seller in a region Anthropic does not serve. The
        // subscription is fine; the route to the vendor is not. Reading this as
        // an auth failure took the whole account off the market under a banner
        // telling its owner to sign in again — which they had, minutes earlier.
        let geo = r#"{"error":{"type":"forbidden","message":"Request not allowed"}}"#;
        assert_eq!(refusal_outcome(403, geo), TaskOutcome::Blocked);
        assert_eq!(
            refusal_outcome(403, r#"{"error":{"code":"unsupported_country_region_territory"}}"#),
            TaskOutcome::Blocked
        );
        // A body that says nothing is not evidence about the credential either.
        assert_eq!(refusal_outcome(403, ""), TaskOutcome::Blocked);

        // A 401 is always the credential, whatever it says.
        assert_eq!(refusal_outcome(401, geo), TaskOutcome::AuthFailed);
        // And a 403 that does talk about permissions keeps its old meaning.
        assert_eq!(
            refusal_outcome(403, r#"{"error":{"type":"permission_error"}}"#),
            TaskOutcome::AuthFailed
        );
        assert_eq!(
            refusal_outcome(403, r#"{"error":{"message":"Invalid API key"}}"#),
            TaskOutcome::AuthFailed
        );
    }

    /// Bodies taken from the 2026-08-17 failures and from the Codex refusal
    /// the probe path already knew about.
    #[test]
    fn a_missing_model_is_told_apart_from_every_other_4xx() {
        use super::unsupported_model;
        assert!(unsupported_model(
            404,
            r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-opus-4-1"}}"#
        ));
        assert!(unsupported_model(
            404,
            r#"{"error":{"message":"The model `gpt-4-turbo-preview` does not exist or you do not have access to it.","code":"model_not_found"}}"#
        ));
        assert!(unsupported_model(404, r#"{"error":{"message":"No endpoints found matching your data policy"}}"#));
        assert!(unsupported_model(
            400,
            r#"{"detail":"gpt-5-codex is not supported when using Codex with a ChatGPT account"}"#
        ));

        // Model Studio, both spellings, as the live endpoint returns them for a
        // resold model the account has not subscribed to.
        assert!(unsupported_model(
            400,
            r#"{"error":{"message":"The product is not activated, please confirm that you have activated products and try again.","code":"invalid_parameter_error"}}"#
        ));
        assert!(unsupported_model(
            403,
            r#"{"error":{"message":"Access denied. For details, see: https://help.aliyun.com/zh/model-studio/error-code#access-denied","type":"access_denied","code":"access_denied"}}"#
        ));
        // A 403 about the credential stays a credential failure — the narrow
        // match above is what keeps these apart.
        assert!(!unsupported_model(403, r#"{"error":{"message":"Invalid API key"}}"#));
        assert!(!unsupported_model(403, r#"{"error":{"code":"access_denied","message":"Access denied"}}"#));

        // The consumer's problem, not the lane's: these must stay a plain 4xx
        // that costs the seller nothing.
        assert!(!unsupported_model(400, r#"{"error":{"message":"messages: at least one message is required"}}"#));
        assert!(!unsupported_model(413, "request entity too large"));
        assert!(!unsupported_model(429, r#"{"error":{"type":"rate_limit_error"}}"#));
        // A bare 404 is a wrong URL — a real mistake on a custom endpoint, and
        // not evidence about the model.
        assert!(!unsupported_model(404, "<html><title>404 Not Found</title></html>"));
    }

    #[test]
    fn the_upstream_model_id_replaces_the_markets_in_the_body() {
        use super::with_model;
        // The lane is declared and metered under the market id; only what goes
        // out is rewritten, and the rest of the body is left exactly as the
        // gateway built it.
        let body = br#"{"model":"claude-haiku-4-5","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
        let patched = with_model(body, "anthropic/claude-haiku-4.5").expect("object body");
        let v: serde_json::Value = serde_json::from_slice(&patched).unwrap();
        assert_eq!(v["model"], "anthropic/claude-haiku-4.5");
        assert_eq!(v["stream"], true);
        assert_eq!(v["messages"][0]["content"], "hi");
        // Not an object: better to fail loudly than to send a body this path
        // does not understand.
        assert!(with_model(b"[1,2]", "x").is_none());
    }

    #[test]
    fn a_custom_base_reaches_chat_completions_however_it_was_pasted() {
        use super::custom_url;
        let built = "https://custom.invalid/v1/chat/completions";
        // The two spellings people actually paste: a vendor's documented base
        // (which ends in /v1) and the same thing with a trailing slash.
        assert_eq!(
            custom_url("https://openrouter.ai/api/v1", Wire::Openai, built),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            custom_url("https://openrouter.ai/api/v1/", Wire::Openai, built),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        // Already the endpoint: appending a second copy would 404 an otherwise
        // correct configuration.
        assert_eq!(
            custom_url("https://host/v1/chat/completions", Wire::Openai, built),
            "https://host/v1/chat/completions"
        );
    }

    #[test]
    fn the_built_route_outranks_a_stale_recorded_dialect() {
        use super::custom_url;
        // One endpoint, two OpenAI routes. The lane's recorded dialect is a
        // snapshot; the path is what the body was actually built for, so a
        // Responses body reaches /responses even on an account still recorded
        // as speaking chat — and the reverse holds too.
        assert_eq!(
            custom_url("https://relay.example/v1", Wire::Openai, "https://custom.invalid/v1/responses"),
            "https://relay.example/v1/responses"
        );
        assert_eq!(
            custom_url("https://relay.example/v1", Wire::Responses, "https://custom.invalid/v1/chat/completions"),
            "https://relay.example/v1/chat/completions"
        );
        // Not a licence to follow any path: the other two dialects have one
        // endpoint each and keep it.
        assert_eq!(
            custom_url("https://relay.example/v1", Wire::Claude, "https://custom.invalid/v1/responses"),
            "https://relay.example/v1/messages"
        );
    }

    #[test]
    fn each_dialect_addresses_its_own_endpoint_under_the_pasted_base() {
        use super::custom_url;
        assert_eq!(
            custom_url("https://relay.example/v1", Wire::Claude, "https://custom.invalid/v1/messages"),
            "https://relay.example/v1/messages"
        );
        assert_eq!(
            custom_url("https://relay.example/v1", Wire::Responses, "https://custom.invalid/v1/responses"),
            "https://relay.example/v1/responses"
        );
        // Gemini is the one whose path carries the request: the model and
        // whether it streams are in it, and neither is anywhere else, so the
        // tail the gateway built has to survive the rewrite intact.
        assert_eq!(
            custom_url(
                "https://relay.example/v1beta",
                Wire::Gemini,
                "https://custom.invalid/v1beta/models/gemini-3.5-flash:streamGenerateContent?alt=sse",
            ),
            "https://relay.example/v1beta/models/gemini-3.5-flash:streamGenerateContent?alt=sse"
        );
    }

    use super::*;
    use crate::protocol::UpstreamPayload;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Mutex;

    struct StaticToken(Option<String>);
    impl TokenProvider for StaticToken {
        fn token_for(&self, _provider: &str) -> Option<String> {
            self.0.clone()
        }
    }

    struct CapturingSink {
        rows: Arc<Mutex<Vec<(String, String, String)>>>, // (model, status, provider)
    }
    #[async_trait]
    impl RecordSink for CapturingSink {
        async fn record(&self, _task: &str, provider: &str, _account: &str, model: &str, _u: &Usage, status: &str) {
            self.rows.lock().await.push((model.to_string(), status.to_string(), provider.to_string()));
        }
    }

    fn body_b64(model: &str) -> String {
        B64.encode(serde_json::to_vec(&json!({"model": model})).unwrap())
    }

    #[test]
    fn usage_is_read_from_every_dialect_including_responses() {
        let cases = [
            // Claude message_delta
            (r#"data: {"type":"message_delta","usage":{"input_tokens":10,"output_tokens":4}}"#, 10, 4),
            // OpenAI chat/completions
            (r#"data: {"usage":{"prompt_tokens":11,"completion_tokens":5}}"#, 11, 5),
            // Gemini
            (r#"data: {"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":6}}"#, 12, 6),
            // Responses: nested under the response object, and null until the end.
            (r#"data: {"type":"response.created","response":{"usage":null}}
data: {"type":"response.completed","response":{"usage":{"input_tokens":13,"output_tokens":7}}}"#, 13, 7),
        ];
        for (frame, input, output) in cases {
            let mut usage = Usage::default();
            accumulate_usage(&mut usage, frame.as_bytes());
            assert_eq!((usage.input_tokens, usage.output_tokens), (input, output), "frame: {frame}");
        }
    }

    /// The real shape of the failure: a Codex `response.completed` frame is
    /// ~1.4 KB, so the network cuts it in half and neither half is JSON. Fed the
    /// raw chunks, the parser reports nothing — which the gateway settles as a
    /// failed, unpaid sale.
    #[test]
    fn a_usage_frame_split_across_chunks_still_counts() {
        let stream = format!(
            "event: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            r#"{"type":"response.output_text.delta","delta":"hello codex"}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":23,"output_tokens":7}}}"#
        );
        let bytes = stream.as_bytes();

        // Cut at every byte: whatever the boundary, the total must come out.
        for cut in 1..bytes.len() {
            let mut scan = UsageScanner::new();
            scan.push(&bytes[..cut]);
            scan.push(&bytes[cut..]);
            let usage = scan.flush();
            assert_eq!(
                (usage.input_tokens, usage.output_tokens),
                (23, 7),
                "usage lost when the stream is cut at byte {cut}"
            );
        }

        // And the bug itself, so this stays a statement about chunking rather
        // than about parsing: cut inside the usage frame and the per-chunk call
        // sees nothing at all.
        let cut = stream.find("\"input_tokens\"").expect("usage frame");
        let mut old = Usage::default();
        accumulate_usage(&mut old, &bytes[..cut]);
        accumulate_usage(&mut old, &bytes[cut..]);
        assert_eq!((old.input_tokens, old.output_tokens), (0, 0));
    }

    /// A last frame with no trailing newline is still counted, and a peer that
    /// never sends one cannot grow the buffer without bound.
    #[test]
    fn the_scanner_flushes_a_trailing_frame_and_caps_a_runaway_line() {
        let mut scan = UsageScanner::new();
        scan.push(br#"data: {"type":"message_delta","usage":{"output_tokens":4}}"#);
        assert_eq!(scan.usage().output_tokens, 0, "held until the line ends or is flushed");
        assert_eq!(scan.flush().output_tokens, 4);

        let mut scan = UsageScanner::new();
        for _ in 0..3 {
            scan.push(&vec![b'x'; MAX_PARTIAL_LINE]);
        }
        assert!(scan.partial.len() <= MAX_PARTIAL_LINE, "fragment is bounded");
    }

    /// Which dialects can tell the guard the stream is over, and which cannot.
    ///
    /// This is what decides whether passing the budget is an error or just a
    /// truncated turn, so it is worth pinning per dialect rather than trusting
    /// one example.
    #[test]
    fn the_scanner_knows_when_a_stream_has_signed_off() {
        let ended = |frames: &[&str]| {
            let mut s = UsageScanner::new();
            for f in frames {
                s.push(format!("{f}\n").as_bytes());
            }
            s.completed()
        };
        // Responses — the only place the Codex backend ever reports usage, and
        // it is the terminal event itself.
        assert!(ended(&[r#"data: {"type":"response.completed","response":{"usage":{"output_tokens":551}}}"#]));
        assert!(ended(&[r#"data: {"type":"response.incomplete","response":{}}"#]));
        assert!(ended(&[r#"data: {"type":"message_stop"}"#]));
        assert!(ended(&["data: [DONE]"]));

        // Mid-flight: Anthropic reports output as it goes, so a runaway is
        // caught while there is still something to cut off.
        assert!(!ended(&[r#"data: {"type":"message_delta","usage":{"output_tokens":900}}"#]));
        assert!(!ended(&[r#"data: {"type":"response.output_text.delta","delta":"hi"}"#]));
        // Gemini never says it is done; "no proof" must not read as "over".
        assert!(!ended(&[r#"data: {"usageMetadata":{"candidatesTokenCount":12}}"#]));

        // A terminal frame split across two transport chunks is still terminal.
        let mut s = UsageScanner::new();
        s.push(br#"data: {"type":"response.comp"#);
        assert!(!s.completed(), "half a line proves nothing");
        s.push(b"leted\",\"response\":{}}\n");
        assert!(s.completed());
    }

    /// The gateway key the tests pretend is pinned into the build.
    fn test_gateway_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[3u8; 32])
    }

    /// A cancel channel that never fires.
    ///
    /// The sender is leaked deliberately: dropping it would resolve the receiver
    /// immediately, and `execute` reads that as "the consumer is gone" — so a
    /// dropped sender here would cancel every call these tests make before it
    /// served anything.
    fn never_canceled() -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::mem::forget(tx);
        rx
    }

    fn test_verifier() -> crate::security::QuotaVerifier {
        crate::security::QuotaVerifier::from_pubkey_b64(&B64.encode(
            test_gateway_key().verifying_key().to_bytes(),
        ))
        .unwrap()
    }

    /// A request carrying a grant genuinely signed by `test_gateway_key`.
    ///
    /// Verification is no longer skippable, so a test that wants to exercise
    /// anything past the quota gate has to present a real signature — which is
    /// the point: there is no configuration in which unsigned work is served.
    fn req(url: &str, model: &str, budget: i64) -> HttpRequestPayload {
        use ed25519_dalek::Signer;
        let exp = 4_000_000_000i64;
        let sig = B64.encode(
            test_gateway_key()
                .sign(format!("t1|{model}|{budget}|{exp}").as_bytes())
                .to_bytes(),
        );
        HttpRequestPayload {
            id: "t1".into(),
            task_id: "t1".into(),
            quota_sig: sig,
            budget_tokens: budget,
            stream: true,
            model: model.to_string(),
            exp,
            upstream: UpstreamPayload {
                provider: "claude".into(),
                method: "POST".into(),
                url: url.to_string(),
                headers: serde_json::Map::new(),
                body_b64: body_b64(model),
            },
        }
    }

    fn drain(out_rx: &mut mpsc::UnboundedReceiver<Envelope>) -> Vec<Envelope> {
        let mut v = Vec::new();
        while let Ok(e) = out_rx.try_recv() {
            v.push(e);
        }
        v
    }

    /// An SSE upstream that sends one delta and then keeps the socket open
    /// forever — a model still generating. Resolves its second return value once
    /// that delta is on the wire, so a test can act at a known point instead of
    /// guessing at a sleep.
    async fn spawn_stalled_sse() -> (String, tokio::sync::oneshot::Receiver<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (wrote_tx, wrote_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let frame = "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"half an answer\"}}\n\n";
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes()).await;
                let _ = sock.flush().await;
                let _ = wrote_tx.send(());
                // Hold the connection open: the stream is still "in progress",
                // which is the state a cancellation has to be able to interrupt.
                std::future::pending::<()>().await;
            }
        });
        (format!("http://127.0.0.1:{port}/"), wrote_rx)
    }

    /// A call the gateway cancels mid-answer stops generating, keeps its metering
    /// row (the subscription quota really was spent), and sends no `stream_end` —
    /// the gateway closed that task's channel when it gave up on the consumer, and
    /// an end frame would claim a completion that never happened.
    #[tokio::test]
    async fn a_canceled_call_stops_mid_stream_and_is_recorded_as_canceled() {
        let (url, wrote) = spawn_stalled_sse().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let rows = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink { rows: rows.clone() };

        tokio::spawn(async move {
            let _ = wrote.await;
            // The delta is on the wire; give the relay a moment to forward it
            // before pulling the rug, so the cancellation lands mid-stream rather
            // than before the first chunk.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = cancel_tx.send(());
        });

        // Returns at all only because the cancellation is honoured: the upstream
        // never ends this stream.
        execute(
            &crate::http::plain(),
            &StaticToken(Some("k".into())),
            req(&url, "claude-sonnet", 0),
            &tx,
            Some(&sink),
            &test_verifier(),
            cancel_rx,
        )
        .await;

        let kinds: Vec<String> = drain(&mut rx).into_iter().map(|e| e.msg_type).collect();
        assert!(kinds.contains(&protocol::T_STREAM_START.to_string()), "frames: {kinds:?}");
        assert!(!kinds.iter().any(|k| k == protocol::T_STREAM_END), "no end frame: {kinds:?}");
        assert!(!kinds.iter().any(|k| k == protocol::T_ERROR), "a cancellation is not an error: {kinds:?}");
        let rows = rows.lock().await;
        assert_eq!(rows.len(), 1, "the call is still metered locally");
        assert_eq!(rows[0].1, "canceled");
    }

    /// A one-shot raw HTTP server that returns a fixed response body.
    async fn spawn_http(response: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://127.0.0.1:{port}/")
    }

    /// An account whose lease table is full for the first `busy_polls` attempts
    /// and has a slot afterwards — the ordinary shape of an over-dispatched
    /// device.
    struct BusyThenFree {
        left: std::sync::Mutex<u32>,
        saturated: bool,
    }
    impl TokenProvider for BusyThenFree {
        fn token_for(&self, _p: &str) -> Option<String> {
            None
        }
        fn acquire(&self, _p: &str, _m: &str) -> Option<LeasedToken> {
            let mut left = self.left.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return None;
            }
            Some(LeasedToken { token: "k".into(), account_id: "dev@example.com".into(), ..Default::default() })
        }
        fn saturated(&self, _p: &str, _m: &str) -> bool {
            self.saturated
        }
    }

    #[tokio::test]
    async fn a_task_waits_for_one_of_this_accounts_slots_instead_of_being_handed_back() {
        // The market dispatches against the concurrency each *lane* declares,
        // and one account's ceiling is shared by every model it sells — so a
        // device selling several models is routinely sent more at once than it
        // said it could take. Answering that with `TOKEN_EXPIRED` told the
        // gateway the credential was broken, and the gateway answered *that*
        // with the cooldown ladder: the lane came off the market over a burst of
        // perfectly ordinary traffic, and model verification — which is a burst
        // by construction — could never get past its second probe.
        let sse = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n";
        let url = spawn_http(sse).await;
        let tokens = BusyThenFree { left: std::sync::Mutex::new(3), saturated: true };

        let (tx, mut rx) = mpsc::unbounded_channel();
        execute(&crate::http::plain(), &tokens, req(&url, "claude-x", 0), &tx, None, &test_verifier(), never_canceled()).await;

        let frames = drain(&mut rx);
        assert!(
            frames.iter().all(|f| f.payload["code"] != "TOKEN_EXPIRED"),
            "a busy account must queue the task, not report a broken credential: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| f.msg_type == protocol::T_STREAM_END),
            "the queued task never completed: {frames:?}"
        );
    }

    #[tokio::test]
    async fn a_lane_with_no_account_behind_it_is_still_reported_at_once() {
        // The other half of the same decision, and the one the wait must not
        // swallow: nothing frees up for a lane no account can serve, so waiting
        // thirty seconds to say so would only hold the buyer's request open for
        // an answer that was available immediately.
        let tokens = BusyThenFree { left: std::sync::Mutex::new(u32::MAX), saturated: false };
        let started = Instant::now();

        let (tx, mut rx) = mpsc::unbounded_channel();
        execute(
            &crate::http::plain(),
            &tokens,
            req("http://127.0.0.1:1/", "claude-x", 0),
            &tx,
            None,
            &test_verifier(),
            never_canceled(),
        )
        .await;

        let frames = drain(&mut rx);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload["code"], "TOKEN_EXPIRED");
        assert!(started.elapsed() < LEASE_WAIT, "an unservable lane must not be waited out");
    }

    #[tokio::test]
    async fn missing_quota_sig_is_rejected() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut r = req("http://127.0.0.1:1/", "claude-x", 0);
        r.quota_sig = "".into();
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), r, &tx, None, &test_verifier(), never_canceled()).await;
        let frames = drain(&mut rx);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload["code"], "QUOTA_SIG_INVALID");
    }

    #[tokio::test]
    async fn forged_quota_grant_is_refused_before_any_upstream_call() {
        use crate::security::QuotaVerifier;
        use ed25519_dalek::{Signer, SigningKey};

        let server = SigningKey::from_bytes(&[3u8; 32]);
        let verifier =
            QuotaVerifier::from_pubkey_b64(&B64.encode(server.verifying_key().to_bytes())).unwrap();

        // A grant signed by somebody who is not the gateway. Port 1 would fail
        // loudly if we ever reached the upstream call.
        let attacker = SigningKey::from_bytes(&[9u8; 32]);
        let exp = 4_000_000_000i64;
        let mut r = req("http://127.0.0.1:1/", "claude-x", 8192);
        r.exp = exp;
        r.quota_sig = B64.encode(
            attacker
                .sign(format!("t1|claude-x|8192|{exp}").as_bytes())
                .to_bytes(),
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), r, &tx, None, &verifier, never_canceled()).await;
        let frames = drain(&mut rx);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload["code"], "QUOTA_SIG_INVALID");
    }

    #[tokio::test]
    async fn valid_quota_grant_is_executed() {
        use crate::security::QuotaVerifier;
        use ed25519_dalek::{Signer, SigningKey};

        let server = SigningKey::from_bytes(&[3u8; 32]);
        let verifier =
            QuotaVerifier::from_pubkey_b64(&B64.encode(server.verifying_key().to_bytes())).unwrap();

        let sse = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n";
        let url = spawn_http(sse).await;
        let exp = 4_000_000_000i64;
        let mut r = req(&url, "claude-x", 8192);
        r.exp = exp;
        r.quota_sig = B64.encode(
            server
                .sign(format!("t1|claude-x|8192|{exp}").as_bytes())
                .to_bytes(),
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), r, &tx, None, &verifier, never_canceled()).await;
        let frames = drain(&mut rx);
        assert_eq!(frames.first().unwrap().msg_type, protocol::T_STREAM_START);
        assert!(frames.iter().any(|f| f.msg_type == protocol::T_STREAM_END));
    }

    #[tokio::test]
    async fn no_token_reports_expired_and_records() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rows = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink { rows: rows.clone() };
        execute(&crate::http::plain(), &StaticToken(None), req("http://127.0.0.1:1/", "claude-x", 0), &tx, Some(&sink), &test_verifier(), never_canceled()).await;
        let frames = drain(&mut rx);
        assert_eq!(frames[0].payload["code"], "TOKEN_EXPIRED");
        let rows = rows.lock().await;
        assert_eq!(rows[0].0, "claude-x"); // model parsed from body
        assert_eq!(rows[0].1, "no_token");
    }

    #[tokio::test]
    async fn streams_sse_and_parses_usage() {
        // SSE body with Claude-style usage; streamed back as stream_start/chunk/end.
        let sse = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11}}}\n\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n\
data: [DONE]\n\n";
        let url = spawn_http(sse).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rows = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink { rows: rows.clone() };
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), req(&url, "claude-sonnet", 0), &tx, Some(&sink), &test_verifier(), never_canceled()).await;
        let frames = drain(&mut rx);
        assert_eq!(frames.first().unwrap().msg_type, protocol::T_STREAM_START);
        let end = frames.iter().find(|f| f.msg_type == protocol::T_STREAM_END).expect("stream_end");
        assert_eq!(end.payload["usage"]["input_tokens"], 11);
        assert_eq!(end.payload["usage"]["output_tokens"], 7);
        assert_eq!(rows.lock().await[0].1, "ok");
    }

    #[tokio::test]
    async fn budget_exceeded_interrupts() {
        let sse = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":500}}\n\n";
        let url = spawn_http(sse).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), req(&url, "claude", 100), &tx, None, &test_verifier(), never_canceled()).await;
        let frames = drain(&mut rx);
        assert!(frames.iter().any(|f| f.payload["code"] == "BUDGET_EXCEEDED"));
    }

    /// The same overshoot, discovered a frame too late to do anything about.
    ///
    /// A Responses stream reports its usage once, in the event that ends it —
    /// the shape every ChatGPT Codex sale has, because that backend refuses the
    /// `max_output_tokens` that would have let it stop by itself. The answer is
    /// already delivered and paid for by the time the guard can see the number,
    /// so an error frame here does not save anyone a token: it only turned a
    /// settled call into `interrupted` and handed the buyer a complete reply
    /// with a fault on the end.
    #[tokio::test]
    async fn a_budget_passed_only_by_the_final_frame_is_a_truncation_not_a_failure() {
        let sse = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"the whole answer\"}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":29,\"output_tokens\":551}}}\n\n";
        let url = spawn_http(sse).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rows = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink { rows: rows.clone() };
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), req(&url, "gpt-5.5", 200), &tx, Some(&sink), &test_verifier(), never_canceled()).await;

        let frames = drain(&mut rx);
        assert!(
            !frames.iter().any(|f| f.payload["code"] == "BUDGET_EXCEEDED"),
            "nothing was cut off, so nothing failed"
        );
        // The buyer keeps every byte, and is told the turn was capped rather
        // than that it ended of its own accord (`normalize_finish`).
        assert!(frames.iter().any(|f| f.msg_type == protocol::T_STREAM_CHUNK));
        let end = frames.iter().find(|f| f.msg_type == protocol::T_STREAM_END).expect("stream_end");
        assert_eq!(end.payload["finish_reason"], "budget");
        assert_eq!(end.payload["usage"]["output_tokens"], 551, "what the subscription actually spent");
        // The seller's own record still says which kind of ending it was.
        assert_eq!(rows.lock().await[0].1, "budget");
    }

    struct ReportingToken {
        outcomes: Arc<std::sync::Mutex<Vec<TaskOutcome>>>,
    }
    impl TokenProvider for ReportingToken {
        fn token_for(&self, _p: &str) -> Option<String> {
            Some("k".into())
        }
        fn acquire(&self, provider: &str, _model: &str) -> Option<LeasedToken> {
            self.token_for(provider)
                .map(|token| LeasedToken { token, account_id: "acc-1".into(), ..Default::default() })
        }
        fn report(&self, _provider: &str, account_id: &str, model: &str, outcome: TaskOutcome) {
            assert_eq!(account_id, "acc-1");
            assert_eq!(model, "claude", "the outcome must be attributed to the lane it came from");
            self.outcomes.lock().unwrap().push(outcome);
        }
    }

    /// Two accounts on one device, the first out of credit.
    ///
    /// `acquire` hands out whichever accounts have not been reported as failed
    /// yet — the pool's cooldown, in miniature.
    struct TwoAccounts {
        failed: Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl TwoAccounts {
        fn next(&self) -> Option<String> {
            let failed = self.failed.lock().unwrap();
            ["acc-broke", "acc-funded"].iter().find(|a| !failed.iter().any(|f| f == *a)).map(|a| a.to_string())
        }
    }
    impl TokenProvider for TwoAccounts {
        fn token_for(&self, _p: &str) -> Option<String> {
            Some("k".into())
        }
        fn acquire(&self, _provider: &str, _model: &str) -> Option<LeasedToken> {
            self.next().map(|account_id| LeasedToken { token: "k".into(), account_id, ..Default::default() })
        }
        fn has_alternate(&self, _p: &str, _m: &str, except: &str) -> bool {
            self.next().is_some_and(|a| a != except)
        }
        fn report(&self, _p: &str, account_id: &str, _m: &str, outcome: TaskOutcome) {
            if !matches!(outcome, TaskOutcome::Success { .. }) {
                self.failed.lock().unwrap().push(account_id.to_string());
            }
        }
    }

    /// An upstream that refuses the first caller and serves the second — one
    /// device's empty aggregator key sitting next to a funded one.
    async fn spawn_http_seq(responses: Vec<&'static str>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://127.0.0.1:{port}/")
    }

    /// A device selling one model through several accounts is a *single* lane
    /// to the market (`{device}|{provider}`), so the gateway's failover ladder
    /// excludes all of them together when one fails. Falling through to the
    /// next account is therefore something only this side can do — and until it
    /// did, one empty key took down every account beside it.
    #[tokio::test]
    async fn an_out_of_credit_account_falls_through_to_the_next_one() {
        let url = spawn_http_seq(vec![
            "HTTP/1.1 402 Payment Required\r\nContent-Type: application/json\r\nContent-Length: 57\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"This request requires more credits\"}}",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"content\":[]}\u{20}\u{20}",
        ])
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let failed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tok = TwoAccounts { failed: failed.clone() };
        execute(&crate::http::plain(), &tok, req(&url, "custom", 0), &tx, None, &test_verifier(), never_canceled()).await;

        let frames = drain(&mut rx);
        assert!(
            frames.iter().all(|f| f.msg_type != protocol::T_ERROR),
            "the buyer must not hear about a failure another account absorbed"
        );
        assert_eq!(failed.lock().unwrap().as_slice(), ["acc-broke"], "and only the empty key is penalised");
    }

    /// The other half of the same rule: a malformed request fails the same way
    /// at every account, so it stops at the first one instead of spending a
    /// second seller's credit to be told the same thing.
    #[tokio::test]
    async fn a_bad_request_is_not_retried_against_another_account() {
        let url = spawn_http_seq(vec![
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 55\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"messages.2: tool_use ids were\"}}",
        ])
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let failed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tok = TwoAccounts { failed: failed.clone() };
        execute(&crate::http::plain(), &tok, req(&url, "custom", 0), &tx, None, &test_verifier(), never_canceled()).await;

        let frames = drain(&mut rx);
        let e = frames.iter().find(|f| f.msg_type == protocol::T_ERROR).unwrap();
        assert_eq!(e.payload["code"], "UPSTREAM_4XX");
        assert!(failed.lock().unwrap().is_empty(), "a bad request is nobody's account's fault");
    }

    #[tokio::test]
    async fn rate_limit_reports_pool_outcome_with_reset() {
        let url = spawn_http(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tok = ReportingToken { outcomes: outcomes.clone() };
        execute(&crate::http::plain(), &tok, req(&url, "claude", 0), &tx, None, &test_verifier(), never_canceled()).await;
        let got = outcomes.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        match got[0] {
            TaskOutcome::RateLimited { reset_at } => assert!(reset_at.is_some(), "Retry-After parsed"),
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn success_reports_tokens_used() {
        let sse = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11}}}\n\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n";
        let url = spawn_http(sse).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tok = ReportingToken { outcomes: outcomes.clone() };
        execute(&crate::http::plain(), &tok, req(&url, "claude", 0), &tx, None, &test_verifier(), never_canceled()).await;
        let got = outcomes.lock().unwrap().clone();
        assert_eq!(got, vec![TaskOutcome::Success { tokens_used: 18 }]);
    }

    /// Anthropic keeps the cached counts beside `input_tokens`, so all three
    /// cross over untouched. Reading only `input_tokens` here is what had every
    /// Claude lane in production reporting an average of 17 prompt tokens.
    #[test]
    fn anthropic_reports_its_cache_counts_alongside_the_prompt() {
        let mut u = super::Usage::default();
        super::accumulate_usage(&mut u, b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":17,\"cache_read_input_tokens\":30000,\"cache_creation_input_tokens\":1024}}}\n");
        super::accumulate_usage(
            &mut u,
            b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":700}}\n",
        );
        assert_eq!(u.input_tokens, 17, "the uncached remainder, as Anthropic means it");
        assert_eq!(u.cache_read_tokens, 30000);
        assert_eq!(u.cache_write_tokens, 1024);
        assert_eq!(u.output_tokens, 700);
        // The window this call really spent, which is what quota decay wants.
        assert_eq!(u.quota_tokens(), 31741);
    }

    /// A `message_delta` that carries no prompt side at all must not reset the
    /// cache counts `message_start` established.
    #[test]
    fn a_later_frame_without_cache_fields_leaves_them_alone() {
        let mut u = super::Usage::default();
        super::accumulate_usage(&mut u, b"data: {\"message\":{\"usage\":{\"input_tokens\":17,\"cache_read_input_tokens\":30000}}}\n");
        super::accumulate_usage(&mut u, b"data: {\"usage\":{\"output_tokens\":700}}\n");
        assert_eq!(u.cache_read_tokens, 30000);
        assert_eq!(u.input_tokens, 17);
    }

    /// The Responses dialect folds the cached tokens into `input_tokens`, so
    /// they have to come back out — billing the full prompt *and* the cache read
    /// charges the buyer twice for the same tokens, at the dearer of the two
    /// rates.
    #[test]
    fn the_responses_dialect_splits_its_cached_tokens_back_out() {
        let mut u = super::Usage::default();
        super::accumulate_usage(&mut u, b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":33000,\"input_tokens_details\":{\"cached_tokens\":30000},\"output_tokens\":129}}}\n");
        assert_eq!(u.input_tokens, 3000, "only the uncached remainder is billed as input");
        assert_eq!(u.cache_read_tokens, 30000);
        assert_eq!(u.output_tokens, 129);
        // The prompt is accounted for exactly once.
        assert_eq!(u.input_tokens + u.cache_read_tokens, 33000);
    }

    /// Same convention in OpenAI's chat dialect, spelled differently.
    #[test]
    fn the_chat_dialect_splits_its_cached_tokens_back_out() {
        let mut u = super::Usage::default();
        super::accumulate_usage(&mut u, b"data: {\"usage\":{\"prompt_tokens\":33000,\"prompt_tokens_details\":{\"cached_tokens\":30000},\"completion_tokens\":129}}\n");
        assert_eq!(u.input_tokens, 3000);
        assert_eq!(u.cache_read_tokens, 30000);
        assert_eq!(u.output_tokens, 129);
    }

    /// A prompt-total dialect that cached nothing still reports its detail
    /// object, and must bill the whole prompt as input.
    #[test]
    fn an_uncached_prompt_total_bills_in_full() {
        let mut u = super::Usage::default();
        super::accumulate_usage(&mut u, b"data: {\"response\":{\"usage\":{\"input_tokens\":900,\"input_tokens_details\":{\"cached_tokens\":0},\"output_tokens\":12}}}\n");
        assert_eq!(u.input_tokens, 900);
        assert_eq!(u.cache_read_tokens, 0);
    }

    #[test]
    fn gemini_splits_its_cached_content_back_out() {
        let mut u = super::Usage::default();
        super::accumulate_usage(&mut u, b"data: {\"usageMetadata\":{\"promptTokenCount\":36393,\"cachedContentTokenCount\":30000,\"candidatesTokenCount\":348}}\n");
        assert_eq!(u.input_tokens, 6393);
        assert_eq!(u.cache_read_tokens, 30000);
        assert_eq!(u.output_tokens, 348);
    }

    /// Gemini bills thinking at the output rate and reports it *outside*
    /// `candidatesTokenCount`. Reading only candidates left this client's own
    /// records short by most of a reasoning turn while the gateway — which had
    /// always read the field — billed the full amount, so the two disagreed
    /// about every Gemini sale.
    #[test]
    fn gemini_thinking_reaches_the_local_record() {
        let mut u = super::Usage::default();
        super::accumulate_usage(
            &mut u,
            br#"data: {"usageMetadata":{"promptTokenCount":1000,"candidatesTokenCount":200,"thoughtsTokenCount":1500,"totalTokenCount":2700}}
"#,
        );
        assert_eq!(u.output_tokens, 1700, "200 visible + 1500 thinking");
        assert_eq!(u.input_tokens, 1000);
    }

    /// The buffered path reads the same shapes without the SSE framing.
    #[test]
    fn a_buffered_body_splits_the_same_way() {
        let u = super::usage_from_body(
            br#"{"usage":{"input_tokens":33000,"input_tokens_details":{"cached_tokens":30000},"output_tokens":129}}"#,
        );
        assert_eq!(u.input_tokens, 3000);
        assert_eq!(u.cache_read_tokens, 30000);
    }

    #[test]
    fn claude_code_preamble_leads_the_system_prompt() {
        let patched = |body: serde_json::Value| -> serde_json::Value {
            let out = with_claude_code_system(&serde_json::to_vec(&body).unwrap(), "")
                .expect("body rewritten");
            serde_json::from_slice(&out).unwrap()
        };

        // No system prompt at all (what a bare relayed body carries).
        let out = patched(json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}));
        let system = out["system"].as_array().unwrap();
        assert_eq!(system.len(), 2, "exactly the billing header and the preamble: {system:?}");
        assert!(system[0]["text"].as_str().unwrap().starts_with(BILLING_PREFIX));
        assert_eq!(system[1]["text"], json!(CLAUDE_CODE_SYSTEM));
        assert_eq!(system[1]["cache_control"]["type"], "ephemeral");

        // The consumer's own prompt keeps its system authority; it only loses
        // the *first* block, which is the one Anthropic reads for the identity.
        // Every model takes the same path — there is no mid-conversation turn
        // to refuse, so no legacy split.
        for model in ["claude-opus-5", "claude-opus-4", "claude-sonnet-4-5-20250929"] {
            let out = patched(json!({
                "model": model,
                "system": "be terse",
                "messages": [{"role": "user", "content": "hi"}],
            }));
            let system = out["system"].as_array().unwrap();
            assert!(system[0]["text"].as_str().unwrap().starts_with(BILLING_PREFIX), "{model}: {system:?}");
            assert_eq!(system[1]["text"], json!(CLAUDE_CODE_SYSTEM), "{model}");
            assert_eq!(system[2]["text"], "be terse", "{model}: caller prompt must stay in the system slot");
            // The breakpoint rides the last block so the caller's prompt is
            // inside the cached prefix, not stranded after it.
            assert!(system[1].get("cache_control").is_none(), "{model}: {system:?}");
            assert_eq!(system[2]["cache_control"]["type"], "ephemeral", "{model}");
            assert!(
                !out["messages"].as_array().unwrap().iter().any(|m| m["role"] == "system"),
                "{model}: no body should get a mid-conversation system turn any more"
            );
        }

        // A body with no user turn still gets the system slot; there is just no
        // conversation to stamp the date into.
        let out = with_claude_code_system(&serde_json::to_vec(&json!({"model": "m"})).unwrap(), "").unwrap();
        let out: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(out["system"].as_array().unwrap().len(), 2);
        assert!(out.get("messages").is_none());
        // Not JSON: nothing to rewrite, and nothing to break.
        assert!(with_claude_code_system(b"not json", "").is_none());
    }

    /// Claude Code's own traffic — the local direct route, and any buyer whose
    /// harness is the CLI — puts the preamble at the head of the *same* block as
    /// its real system prompt. Only the preamble is ours to re-emit; the rest is
    /// the caller's instructions, and losing them sends the model a bare
    /// "you are Claude Code" and nothing else.
    #[test]
    fn a_compliant_callers_own_instructions_survive_the_preamble_strip() {
        let body = json!({
            "model": "claude-opus-5",
            "system": [{"type": "text", "text": format!("{CLAUDE_CODE_SYSTEM}\n\nNever edit files without reading them.")}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let out = with_claude_code_system(&serde_json::to_vec(&body).unwrap(), "ses-1").unwrap();
        let out: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let raw = serde_json::to_string(&out).unwrap();
        assert!(raw.contains("Never edit files without reading them."), "caller instructions lost: {raw}");
        // The preamble is re-emitted as its own leading block and the caller's
        // tail follows it, rather than the two sharing one block as the CLI
        // sends them — same text, same order, one block boundary more.
        let system = out["system"].as_array().unwrap();
        assert_eq!(system.len(), 3, "{system:?}");
        assert_eq!(system[2]["text"], "Never edit files without reading them.");
    }

    /// Anthropic refuses a request carrying a fifth cache breakpoint, and the
    /// relayed body's own are not ours to drop — so the two this file adds are
    /// the ones that give way. A missed breakpoint re-reads a prefix; a `400`
    /// costs the turn and cools the lane.
    #[test]
    fn a_body_that_already_spends_every_breakpoint_gets_no_more() {
        let bp = json!({"type": "ephemeral"});
        let body = json!({
            "model": "claude-opus-5",
            "system": [{"type": "text", "text": "be terse", "cache_control": bp}],
            "tools": [{"name": "t", "cache_control": bp}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "one", "cache_control": bp}]},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": [{"type": "text", "text": "two", "cache_control": bp}]},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": [{"type": "text", "text": "three", "cache_control": bp}]},
            ],
        });
        let out = with_claude_code_system(&serde_json::to_vec(&body).unwrap(), "ses-1").unwrap();
        let out: serde_json::Value = serde_json::from_slice(&out).unwrap();
        // Four already spent by tools and messages, so the system slot — whose
        // own was dropped with the caller's blocks — gets none back.
        assert_eq!(super::spent_breakpoints(&out), 4, "the caller's own were kept");
        assert!(
            out["system"].as_array().unwrap().iter().all(|b| b.get("cache_control").is_none()),
            "a fifth breakpoint would 400 the whole turn: {}",
            out["system"]
        );

        // One slot free: the system prefix is worth caching, so it is taken.
        let body = json!({
            "model": "claude-opus-5",
            "tools": [{"name": "t", "cache_control": bp}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        });
        let out = with_claude_code_system(&serde_json::to_vec(&body).unwrap(), "ses-1").unwrap();
        let out: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(super::spent_breakpoints(&out) <= 4);
        assert!(
            serde_json::to_string(&out["system"]).unwrap().contains("cache_control"),
            "room to spare and the prefix went uncached: {}",
            out["system"]
        );
    }

    /// A caller's own `metadata.user_id` is kept only when it is one Claude
    /// Code would have written. Anything else — including the old
    /// `user_x_session_y` spelling the CLI retired long before the 2.1.220 the
    /// billing header claims — is a third-party tell, so it is replaced.
    #[test]
    fn only_a_claude_code_shaped_user_id_survives() {
        let with = |user_id: &str| -> String {
            let body = json!({
                "model": "claude-opus-5",
                "metadata": {"user_id": user_id},
                "messages": [{"role": "user", "content": "hi"}],
            });
            let out = with_claude_code_system(&serde_json::to_vec(&body).unwrap(), "ses-1").unwrap();
            let out: serde_json::Value = serde_json::from_slice(&out).unwrap();
            out["metadata"]["user_id"].as_str().unwrap().to_string()
        };

        // A real one from the CLI: kept verbatim.
        let native = json!({
            "device_id": "a".repeat(64),
            "account_uuid": "",
            "session_id": "3f2504e0-4f89-11d3-9a0c-0305e82c3301",
        })
        .to_string();
        assert_eq!(with(&native), native);

        // The retired shape: replaced with one that parses.
        let replaced = with("user_real_session_abc");
        assert_ne!(replaced, "user_real_session_abc");
        assert!(is_claude_code_user_id(&replaced), "replacement is not CLI-shaped: {replaced}");
    }

    #[test]
    fn claude_code_metadata_keeps_a_stable_session() {
        let body = json!({"model": "claude-opus-5", "messages": [{"role": "user", "content": "hi"}]});
        let (a, b) = (
            with_claude_code_system(&serde_json::to_vec(&body).unwrap(), "ses-1").unwrap(),
            with_claude_code_system(&serde_json::to_vec(&body).unwrap(), "ses-1").unwrap(),
        );
        let (a, b): (serde_json::Value, serde_json::Value) = (serde_json::from_slice(&a).unwrap(), serde_json::from_slice(&b).unwrap());
        let ua = a.pointer("/metadata/user_id").and_then(|v| v.as_str());
        let ub = b.pointer("/metadata/user_id").and_then(|v| v.as_str());
        assert_eq!(ua, ub, "the same account must keep the same session");
        let ua = ua.unwrap();
        assert!(is_claude_code_user_id(ua), "user_id is not the shape Claude Code sends: {ua}");
        assert!(ua.contains(&session_uuid("ses-1")), "the account's own session must be the one carried: {ua}");
    }

    /// The build hash behind `cc_version` is Claude Code's own algorithm, not a
    /// plausible-looking one: salt, the 4th/7th/20th character of the hashed
    /// user text, the version, sha256, first three hex digits. Anchored against
    /// CLIProxyAPI's `computeFingerprint`.
    #[test]
    fn the_billing_fingerprint_matches_the_cli() {
        let expect = |text: &str| {
            let chars: Vec<char> = text.chars().collect();
            let picked: String = [4usize, 7, 20].iter().map(|&i| chars.get(i).copied().unwrap_or('0')).collect();
            let h = format!("{:x}", sha2::Sha256::digest(format!("59cf53e54c78{picked}2.1.220").as_bytes()));
            h[..3].to_string()
        };
        for text in ["", "hi", "the quick brown fox jumps over the lazy dog"] {
            assert_eq!(fingerprint_hash(text.to_string()), expect(text), "text {text:?}");
        }

        // The hashed text is the *first* user turn that has text, and within it
        // the last text part — a leading tool-result-only turn falls through.
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "42"}]},
            {"role": "user", "content": [{"type": "text", "text": "first"}, {"type": "text", "text": "second"}]},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": [{"type": "text", "text": "third"}]},
        ]});
        assert_eq!(first_user_text(&body), "second");
    }

    /// What the cache actually needs: everything ahead of the last breakpoint
    /// must be byte-identical from one turn of a conversation to the next.
    /// Hashing the latest user turn into `system[0]` broke exactly this, and a
    /// prefix that moves every turn is re-billed at the cache-*write* rate
    /// forever — 482 production relays, 204 writes, zero reads.
    #[test]
    fn the_cached_prefix_survives_the_next_turn() {
        let turn = |msgs: serde_json::Value| {
            let body = json!({"model": "claude-fable-5", "system": "be terse", "messages": msgs});
            let out = with_claude_code_system(&serde_json::to_vec(&body).unwrap(), "ses-1").unwrap();
            serde_json::from_slice::<serde_json::Value>(&out).unwrap()["system"].clone()
        };
        let first = turn(json!([{"role": "user", "content": "how do I list files"}]));
        let second = turn(json!([
            {"role": "user", "content": "how do I list files"},
            {"role": "assistant", "content": "ls"},
            {"role": "user", "content": "and hidden ones"},
        ]));
        assert_eq!(first, second, "the cached system prefix moved between turns");

        // Still per-conversation, not one global constant — a different opener
        // is a different prefix, which is what the CLI's own hash gives.
        let other = turn(json!([{"role": "user", "content": "write me a haiku"}]));
        assert_ne!(first, other, "every conversation collapsed onto one fingerprint");
    }

    /// The beta list tracks what the body actually holds, and the identity beta
    /// leads it whatever that is.
    #[test]
    fn the_beta_list_follows_the_body() {
        let betas = |body: serde_json::Value| claude_code_betas(&serde_json::to_vec(&body).unwrap(), true);

        let modern = betas(json!({"model": "claude-opus-5", "tools": [{"name": "t"}]}));
        assert!(modern.starts_with("claude-code-20250219,oauth-2025-04-20,"), "{modern}");
        assert!(modern.contains("advanced-tool-use-2025-11-20"), "{modern}");

        // No body writes a mid-conversation `role: system` turn any more, so
        // declaring the flag that unlocks one is a claim about a shape this
        // client no longer sends.
        for model in ["claude-opus-5", "claude-opus-4", "claude-sonnet-4-5-20250929"] {
            assert!(!betas(json!({"model": model})).contains("mid-conversation-system"), "{model}");
        }

        // Reasoning the buyer paid for is never redacted away.
        let thinking = betas(json!({"model": "claude-opus-5", "thinking": {"type": "enabled", "budget_tokens": 2048}}));
        assert!(!thinking.contains("redact-thinking"), "{thinking}");

        // An API-key credential drops the three OAuth-scoped flags.
        let api_key = claude_code_betas(&serde_json::to_vec(&json!({"model": "claude-opus-5"})).unwrap(), false);
        for flag in ["oauth-2025-04-20", "fallback-credit-2026-06-01", "extended-cache-ttl-2025-04-11"] {
            assert!(!api_key.contains(flag), "{flag} leaked onto an api-key request: {api_key}");
        }
    }

    /// The version in the billing header and the one in the user-agent are the
    /// same claim; a request that makes them disagree is the mismatch the whole
    /// fingerprint exists to avoid.
    #[test]
    fn the_user_agent_and_the_billing_version_agree() {
        let ua = asale_protocol::spec(Provider::Claude).user_agent;
        assert!(ua.contains(CLAUDE_CODE_VERSION), "{ua} does not claim {CLAUDE_CODE_VERSION}");
    }

    #[tokio::test]
    async fn claude_upstream_call_identifies_as_claude_code() {
        // Anthropic answers a subscription request that does not present itself
        // as Claude Code with a masked 429, which the pool would read as an
        // exhausted window and take the whole account off the market for.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let sink = seen.clone();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                *sink.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                let _ = sock.flush().await;
            }
        });

        let (tx, _rx) = mpsc::unbounded_channel();
        let url = format!("http://127.0.0.1:{port}/");
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), req(&url, "claude-opus-5", 0), &tx, None, &test_verifier(), never_canceled())
            .await;
        let _ = server.await;

        let raw = seen.lock().unwrap().clone();
        assert!(raw.contains(CLAUDE_CODE_SYSTEM), "system preamble missing from the wire: {raw}");
        let lower = raw.to_lowercase();
        for beta in ["claude-code-20250219", "oauth-2025-04-20"] {
            assert!(lower.contains(beta), "{beta} missing from the wire: {raw}");
        }
        assert_eq!(lower.matches("anthropic-beta:").count(), 1, "one beta header, not two: {raw}");
        assert!(lower.contains("x-app: cli"), "SDK identity headers missing: {raw}");
        assert!(lower.contains(&format!("user-agent: {}", asale_protocol::spec(Provider::Claude).user_agent.to_lowercase())), "{raw}");
    }

    /// A leased Codex token whose account id is known.
    struct CodexToken(Option<&'static str>);
    impl TokenProvider for CodexToken {
        fn token_for(&self, _p: &str) -> Option<String> {
            Some("k".into())
        }
        fn acquire(&self, _p: &str, _m: &str) -> Option<LeasedToken> {
            Some(LeasedToken {
                token: "k".into(),
                account_id: "dev@example.com".into(),
                session_id: None,
                upstream_account_id: self.0.map(String::from),
                upstream_base: None,
                upstream_wire: None,
                upstream_model: None,
            })
        }
    }

    fn codex_req(url: &str) -> HttpRequestPayload {
        let mut r = req(url, "gpt-5.1", 0);
        r.upstream.provider = "codex".into();
        r
    }

    /// A bearer shaped like a real ChatGPT access token: `header.claims.sig`,
    /// url-safe base64, no padding.
    fn chatgpt_jwt(account: &str) -> String {
        let claims = json!({"https://api.openai.com/auth": {"chatgpt_account_id": account}});
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        format!("h.{b64}.s")
    }

    /// A leased Codex token with nothing stored alongside it — the state every
    /// account connected before asale started recording the id is in.
    struct CodexTokenNoStoredId(String);
    impl TokenProvider for CodexTokenNoStoredId {
        fn token_for(&self, _p: &str) -> Option<String> {
            Some(self.0.clone())
        }
        fn acquire(&self, _p: &str, _m: &str) -> Option<LeasedToken> {
            Some(LeasedToken {
                token: self.0.clone(),
                account_id: "dev@example.com".into(),
                session_id: None,
                upstream_account_id: None,
                upstream_base: None,
                upstream_wire: None,
                upstream_model: None,
            })
        }
    }

    /// The bearer names its own account, so an already-connected Codex account
    /// must not need a fresh sign-in just to recover a value it is already
    /// carrying on every request.
    #[tokio::test]
    async fn codex_falls_back_to_the_account_id_inside_the_bearer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let sink = seen.clone();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                *sink.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                let _ = sock.flush().await;
            }
        });

        let (tx, _rx) = mpsc::unbounded_channel();
        let url = format!("http://127.0.0.1:{port}/");
        let tokens = CodexTokenNoStoredId(chatgpt_jwt("acc-from-jwt"));
        execute(&crate::http::plain(), &tokens, codex_req(&url), &tx, None, &test_verifier(), never_canceled()).await;
        let _ = server.await;

        let raw = seen.lock().unwrap().to_lowercase();
        assert!(raw.contains("chatgpt-account-id: acc-from-jwt"), "id not recovered from the bearer: {raw}");
    }

    /// The stored value is the authoritative one: it comes from the id_token the
    /// vendor issued for this account, so it wins over anything inferred.
    #[tokio::test]
    async fn a_stored_account_id_beats_the_one_in_the_bearer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let sink = seen.clone();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                *sink.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                let _ = sock.flush().await;
            }
        });

        struct Both(String);
        impl TokenProvider for Both {
            fn token_for(&self, _p: &str) -> Option<String> {
                Some(self.0.clone())
            }
            fn acquire(&self, _p: &str, _m: &str) -> Option<LeasedToken> {
                Some(LeasedToken {
                    token: self.0.clone(),
                    account_id: "dev@example.com".into(),
                    session_id: None,
                    upstream_account_id: Some("acc-stored".into()),
                upstream_base: None,
                upstream_wire: None,
                upstream_model: None,
                })
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let url = format!("http://127.0.0.1:{port}/");
        execute(&crate::http::plain(), &Both(chatgpt_jwt("acc-from-jwt")), codex_req(&url), &tx, None, &test_verifier(), never_canceled())
            .await;
        let _ = server.await;

        let raw = seen.lock().unwrap().to_lowercase();
        assert!(raw.contains("chatgpt-account-id: acc-stored"), "stored id must win: {raw}");
        assert!(!raw.contains("acc-from-jwt"));
    }

    /// The ChatGPT backend authenticates the bearer *and* the account id it was
    /// issued for. Sending only the bearer is a 401, which the pool reads as a
    /// dead login — so this header is the difference between a Codex account
    /// that sells and one that looks permanently signed out.
    #[tokio::test]
    async fn codex_sends_the_chatgpt_account_id_with_the_bearer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let sink = seen.clone();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                *sink.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                let _ = sock.flush().await;
            }
        });

        let (tx, _rx) = mpsc::unbounded_channel();
        let url = format!("http://127.0.0.1:{port}/");
        execute(&crate::http::plain(), &CodexToken(Some("acc-1")), codex_req(&url), &tx, None, &test_verifier(), never_canceled()).await;
        let _ = server.await;

        let raw = seen.lock().unwrap().to_lowercase();
        assert!(raw.contains("chatgpt-account-id: acc-1"), "account id missing from the wire: {raw}");
        // Claude's OAuth requirements are Claude's; they must not follow along.
        assert!(!raw.contains("anthropic-beta"), "claude headers leaked onto codex: {raw}");
    }

    /// The shape has to name the roles and carry no prompt text — that pairing is
    /// the whole point of logging it. The body here is the one that draws
    /// `400 {"detail":"System messages are not allowed"}` out of the ChatGPT
    /// backend, and the fingerprint has to make the offending item visible.
    #[test]
    fn body_shape_names_the_roles_without_the_text() {
        let shape = body_shape(
            json!({
                "model": "gpt-5.6-terra",
                "instructions": "You are Codex.",
                "input": [
                    {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "SECRET PROMPT"}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "SECRET ASK"}]},
                    {"type": "function_call_output", "call_id": "c1", "output": "SECRET RESULT"}
                ],
                "tools": [{"type": "function", "name": "grep"}],
                "stream": true
            })
            .to_string()
            .as_bytes(),
        );
        assert!(shape.contains("input=[message:system message:user function_call_output]"), "{shape}");
        assert!(shape.contains("instructions=14B"), "{shape}");
        assert!(shape.contains("tools=1"), "{shape}");
        assert!(shape.contains("keys=[input,instructions,model,stream,tools]"), "{shape}");
        for secret in ["SECRET PROMPT", "SECRET ASK", "SECRET RESULT", "You are Codex"] {
            assert!(!shape.contains(secret), "prompt text leaked into the log: {shape}");
        }
    }

    /// A body the upstream refused *and* could not parse still has to say
    /// something — a publisher whose gateway sent garbage sees only this line.
    #[test]
    fn body_shape_survives_a_non_json_body() {
        assert_eq!(body_shape(b"not json at all"), "non-json 15B");
    }

    /// Without the id the request can only 401, and a 401 here is indistinguishable
    /// from a revoked login — so fail with a message that names the actual fix
    /// instead of letting the pool mislabel the account.
    #[tokio::test]
    async fn codex_without_an_account_id_fails_before_the_call() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Port 1 would fail loudly if the request were ever sent.
        execute(&crate::http::plain(), &CodexToken(None), codex_req("http://127.0.0.1:1/"), &tx, None, &test_verifier(), never_canceled())
            .await;
        let frames = drain(&mut rx);
        assert_eq!(frames[0].payload["code"], "TOKEN_EXPIRED");
        assert_eq!(frames[0].payload["retriable"], true, "one machine's missing config is not the buyer's failure");
        assert!(frames[0].payload["message"].as_str().unwrap().contains("chatgpt-account-id"));
    }

    #[tokio::test]
    async fn a_plain_429_is_reported_as_a_rate_limit() {
        let url = spawn_http("HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), req(&url, "claude", 0), &tx, None, &test_verifier(), never_canceled()).await;
        let frames = drain(&mut rx);
        let e = frames.iter().find(|f| f.msg_type == protocol::T_ERROR).unwrap();
        // Not `UPSTREAM_4XX`: that is the buyer's request being wrong, and it
        // leaves the rate-limited lane advertised for the next buyer to hit.
        assert_eq!(e.payload["code"], "UPSTREAM_RATE_LIMIT");
        assert_eq!(e.payload["retriable"], true);
    }

    #[test]
    fn third_party_usage_wall_cools_the_account() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Third-party apps now draw from your extra usage, not your plan limits. Add more at claude.ai/settings/usage and keep going."}}"#;
        assert!(quota_exhausted(400, body));
        // Ordinary 400s stay the consumer's problem.
        assert!(!quota_exhausted(400, r#"{"error":{"message":"max_tokens: 99999 > 32000"}}"#));
    }

    /// The same wall, in the other vocabularies sellers actually hit. Each of
    /// these was reaching the buyer as `UPSTREAM_4XX` — "your request was
    /// bad" — while the lane stayed advertised and kept winning matches.
    #[test]
    fn an_empty_aggregator_balance_is_the_same_wall() {
        assert!(quota_exhausted(
            400,
            r#"{"error":{"message":"credit insufficient balance: balance=1010857 required=1227326","code":"insufficient_user_quota"}}"#
        ));
        assert!(quota_exhausted(
            402,
            r#"{"error":{"message":"This request requires more credits, or fewer max_tokens.","code":402}}"#
        ));
        assert!(quota_exhausted(
            402,
            r#"{"error":{"message":"This request would exceed your available credits given your current in-flight requests."}}"#
        ));
        // Still not every 4xx: a malformed request is the buyer's to fix, and
        // cooling the seller for it takes a working lane off the market.
        assert!(!quota_exhausted(400, r#"{"error":{"message":"messages.2: tool_use ids were found"}}"#));
        assert!(!quota_exhausted(404, r#"{"error":{"message":"No endpoints found for openai/gpt-5.2-chat."}}"#));
    }

    #[tokio::test]
    async fn the_usage_wall_is_handed_to_another_seller_and_reported_as_a_limit() {
        let url = spawn_http(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 118\r\nConnection: close\r\n\r\n{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"Third-party apps now draw from your extra usage.\"}}",
        )
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tok = ReportingToken { outcomes: outcomes.clone() };
        execute(&crate::http::plain(), &tok, req(&url, "claude", 0), &tx, None, &test_verifier(), never_canceled()).await;

        let frames = drain(&mut rx);
        let e = frames.iter().find(|f| f.msg_type == protocol::T_ERROR).unwrap();
        // Not `UPSTREAM_4XX`: that code says "the consumer's request was bad",
        // which strands the buyer on a seller that cannot answer anyone.
        assert_eq!(e.payload["code"], "UPSTREAM_RATE_LIMIT");
        assert_eq!(e.payload["retriable"], true, "the buyer must reach another seller");
        assert_eq!(
            outcomes.lock().unwrap().as_slice(),
            [TaskOutcome::QuotaExhausted { reset_at: None }],
            "and the account is cooled, not credited with a success"
        );
    }

    #[tokio::test]
    async fn an_ordinary_400_still_belongs_to_the_consumer() {
        let url = spawn_http(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 49\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"max_tokens: 99999 > 32000\"}}",
        )
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tok = ReportingToken { outcomes: outcomes.clone() };
        execute(&crate::http::plain(), &tok, req(&url, "claude", 0), &tx, None, &test_verifier(), never_canceled()).await;

        let e = drain(&mut rx).into_iter().find(|f| f.msg_type == protocol::T_ERROR).unwrap();
        assert_eq!(e.payload["code"], "UPSTREAM_4XX");
        assert_eq!(e.payload["retriable"], false, "a bad request fails the same way everywhere");
        assert_eq!(outcomes.lock().unwrap().as_slice(), [TaskOutcome::Success { tokens_used: 0 }]);
    }
}
