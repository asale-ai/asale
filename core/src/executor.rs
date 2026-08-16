//! Provider executor (spec §5.2). On an `http_request`, inject the local
//! subscription token, call the upstream, and stream `stream_start/chunk/end`
//! frames back. The token is injected here and nowhere else — the server and
//! consumer never see it.

use crate::protocol::{self, Envelope, HttpRequestPayload, Usage};
use asale_protocol::ids::Wire;
use crate::security::QuotaVerifier;
use async_trait::async_trait;
use base64::Engine;
use futures_util::StreamExt;
use serde_json::json;
use std::collections::BTreeMap;
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

/// A leased token: the bearer plus the pool account it came from (empty
/// account_id when the provider has no pool semantics).
#[derive(Debug, Clone, Default)]
pub struct LeasedToken {
    pub token: String,
    pub account_id: String,
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
}

/// How to resolve the upstream bearer token for a provider.
///
/// Both hooks carry the model: availability and failure state are tracked per
/// `(account, model)` lane, so an account that is fine for one model and
/// broken for another can be described accurately (spec §4.5).
pub trait TokenProvider: Send + Sync {
    /// Return the bearer token for a provider (e.g. "claude"), or None.
    fn token_for(&self, provider: &str) -> Option<String>;

    /// Lease a token for one lane. Pool-backed implementations pick an account
    /// whose lane for `model` is serving (spec §4); the default wraps
    /// `token_for` with no account identity.
    fn acquire(&self, provider: &str, _model: &str) -> Option<LeasedToken> {
        self.token_for(provider).map(|token| LeasedToken { token, ..Default::default() })
    }

    /// Report the outcome of a leased call. Default: no-op.
    fn report(&self, _provider: &str, _account_id: &str, _model: &str, _outcome: TaskOutcome) {}
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
/// Empty for every provider that does not report its rate-limit state this way
/// — which today is all of them but Codex, whose `x-codex-*` block is the only
/// reading a ChatGPT bearer can get at all (`/backend-api/codex/usage` answers
/// that credential 403).
pub fn quota_headers(provider: &str, headers: &reqwest::header::HeaderMap) -> BTreeMap<String, String> {
    let prefix = match provider {
        "codex" => "x-codex-",
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
        send_error(out, &task_id, "QUOTA_SIG_INVALID", "missing quota grant signature", false);
        return;
    }
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Err(e) = quota.verify(&req.task_id, &req.model, req.budget_tokens, req.exp, &req.quota_sig, now) {
            tracing::warn!(task = %task_id, "refusing relayed request: {e}");
            send_error(out, &task_id, "QUOTA_SIG_INVALID", &e.to_string(), false);
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
    let lease = match tokens.acquire(&provider, &model) {
        Some(l) => l,
        None => {
            send_error(out, &task_id, "TOKEN_EXPIRED", "no local token for provider", true);
            if let Some(r) = records {
                r.record(&task_id, &provider, "", &model, &Usage::default(), "no_token").await;
            }
            return;
        }
    };
    let token = lease.token.clone();

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
    for (k, v) in &req.upstream.headers {
        if let Some(s) = v.as_str() {
            builder = builder.header(k, s);
        }
    }
    builder = match custom {
        Some((_, wire)) => authorize_custom(builder, wire, &token, &req.upstream.headers),
        None => builder.header("authorization", format!("Bearer {token}")),
    };
    let mut body = B64.decode(req.upstream.body_b64.as_bytes()).unwrap_or_default();
    // The token we just injected is a Claude Code subscription credential, and
    // the server that built this body does not know that — so the OAuth-only
    // requirements are applied here, where the credential is known.
    if is_claude(&provider) {
        if !req.upstream.headers.keys().any(|k| k.eq_ignore_ascii_case("anthropic-beta")) {
            builder = builder.header("anthropic-beta", CLAUDE_OAUTH_BETA);
        }
        if let Some(patched) = with_claude_code_system(&body) {
            body = patched;
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
                    false,
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
            send_error(out, &task_id, "UPSTREAM_5XX", &format!("upstream send: {e}"), true);
            if let Some(r) = records {
                r.record(&task_id, &provider, &lease.account_id, &model, &Usage::default(), "upstream_error").await;
            }
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
        let code = if status < 500 { "UPSTREAM_4XX" } else { "UPSTREAM_5XX" };
        let retriable = status >= 500 || status == 429;
        // Pool feedback: 429 cools the account (honoring Retry-After), 5xx
        // applies the transient cooldown, 401/403 flags the token (spec §4).
        let outcome = match status {
            429 => TaskOutcome::RateLimited { reset_at: retry_after_reset(&resp) },
            401 | 403 => TaskOutcome::AuthFailed,
            s if s >= 500 => TaskOutcome::ServerError,
            _ => TaskOutcome::Success { tokens_used: 0 },
        };
        tokens.report(&provider, &lease.account_id, &model, outcome);
        // The upstream's own words never leave this process, but they are the
        // only way to tell a real quota exhaustion from a rejection wearing a
        // 429 (Anthropic masks OAuth policy failures as `rate_limit_error`), so
        // keep them in the log rather than dropping them on the floor.
        let detail = resp.text().await.unwrap_or_default();
        tracing::warn!(
            task = %task_id, provider = %provider, model = %model, status, sent = %shape,
            "upstream rejected: {}", detail.chars().take(400).collect::<String>()
        );
        send_error(out, &task_id, code, &format!("upstream {status}"), retriable);
        if let Some(r) = records {
            r.record(&task_id, &provider, &lease.account_id, &model, &Usage::default(), &format!("upstream_{status}")).await;
        }
        return;
    }

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
            send_error(out, &task_id, "BUDGET_EXCEEDED", "output exceeded budget", false);
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
    provider == "claude" || provider == "claude_work"
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

/// Put Claude Code's own preamble back at the head of the system prompt.
///
/// Anthropic only accepts subscription (OAuth) traffic that identifies itself
/// as Claude Code: a request whose system prompt does not open with this line
/// is refused with `429 {"type":"rate_limit_error"}` — a masked policy failure,
/// not an exhausted window. Relayed market requests carry the *consumer's*
/// system prompt (or none at all), so without this every sale 429s, and the
/// pool then cools the whole account for a limit it never actually hit.
///
/// The caller's own prompt is kept as the block behind the preamble, so what
/// the buyer asked for still reaches the model. Returns `None` when the body is
/// not JSON or already complies, i.e. when there is nothing to rewrite.
pub fn with_claude_code_system(body: &[u8]) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object_mut()?;
    let preamble = json!({"type": "text", "text": CLAUDE_CODE_SYSTEM});
    let system = match obj.get("system") {
        // No system prompt at all — the preamble is the whole of it.
        None | Some(serde_json::Value::Null) => json!([preamble]),
        Some(serde_json::Value::String(s)) => {
            if s.starts_with(CLAUDE_CODE_SYSTEM) {
                return None;
            }
            json!([preamble, json!({"type": "text", "text": s})])
        }
        Some(serde_json::Value::Array(blocks)) => {
            let first_ok = blocks
                .first()
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.starts_with(CLAUDE_CODE_SYSTEM));
            if first_ok {
                return None;
            }
            let mut out = vec![preamble];
            out.extend(blocks.iter().cloned());
            json!(out)
        }
        // Anything else is a shape Anthropic would reject anyway; leave it.
        Some(_) => return None,
    };
    obj.insert("system".into(), system);
    serde_json::to_vec(&v).ok()
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

fn send_error(out: &mpsc::UnboundedSender<Envelope>, task_id: &str, code: &str, message: &str, retriable: bool) {
    let _ = out.send(Envelope::with_id(
        task_id,
        protocol::T_ERROR,
        json!({"id": task_id, "task_id": task_id, "code": code, "message": message, "retriable": retriable}),
    ));
}

#[cfg(test)]
mod tests {
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
            let out = with_claude_code_system(&serde_json::to_vec(&body).unwrap())
                .expect("body rewritten");
            serde_json::from_slice(&out).unwrap()
        };

        // No system prompt at all (what a bare relayed body carries).
        assert_eq!(
            patched(json!({"model": "m"}))["system"],
            json!([{"type": "text", "text": CLAUDE_CODE_SYSTEM}])
        );
        // The consumer's own prompt survives, behind the preamble.
        assert_eq!(
            patched(json!({"system": "be terse"}))["system"],
            json!([
                {"type": "text", "text": CLAUDE_CODE_SYSTEM},
                {"type": "text", "text": "be terse"},
            ])
        );
        assert_eq!(
            patched(json!({"system": [{"type": "text", "text": "be terse"}]}))["system"],
            json!([
                {"type": "text", "text": CLAUDE_CODE_SYSTEM},
                {"type": "text", "text": "be terse"},
            ])
        );
        // Already compliant (Claude Code's own traffic) — left untouched, in
        // either shape, so the direct route never doubles the preamble.
        for body in [
            json!({"system": CLAUDE_CODE_SYSTEM}),
            json!({"system": [{"type": "text", "text": format!("{CLAUDE_CODE_SYSTEM} More rules.")}]}),
        ] {
            assert!(
                with_claude_code_system(&serde_json::to_vec(&body).unwrap()).is_none(),
                "compliant body must not be rewritten: {body}"
            );
        }
        // Not JSON: nothing to rewrite, and nothing to break.
        assert!(with_claude_code_system(b"not json").is_none());
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
        assert!(raw.to_lowercase().contains("anthropic-beta: oauth-2025-04-20"), "oauth beta flag missing: {raw}");
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
        assert_eq!(frames[0].payload["retriable"], false);
        assert!(frames[0].payload["message"].as_str().unwrap().contains("chatgpt-account-id"));
    }

    #[tokio::test]
    async fn upstream_4xx_reports_error() {
        let url = spawn_http("HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), req(&url, "claude", 0), &tx, None, &test_verifier(), never_canceled()).await;
        let frames = drain(&mut rx);
        let e = frames.iter().find(|f| f.msg_type == protocol::T_ERROR).unwrap();
        assert_eq!(e.payload["code"], "UPSTREAM_4XX");
        assert_eq!(e.payload["retriable"], true); // 429 is retriable
    }
}
