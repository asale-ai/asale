//! Provider executor (spec §5.2). On an `http_request`, inject the local
//! subscription token, call the upstream, and stream `stream_start/chunk/end`
//! frames back. The token is injected here and nowhere else — the server and
//! consumer never see it.

use crate::protocol::{self, Envelope, HttpRequestPayload, Usage};
use crate::security::QuotaVerifier;
use async_trait::async_trait;
use base64::Engine;
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

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
}

/// Execute one relayed request and stream results back via `out`.
///
/// `quota` is built from the key pinned into this build
/// ([`crate::security::pinned_quota_verifier`]) — never from anything the
/// gateway said on this connection. It is not optional: a device with no way to
/// check a grant does not serve at all, so this function is only reachable once
/// a verifier exists.
pub async fn execute(
    http: &reqwest::Client,
    tokens: &dyn TokenProvider,
    req: HttpRequestPayload,
    out: &mpsc::UnboundedSender<Envelope>,
    records: Option<&dyn RecordSink>,
    quota: &QuotaVerifier,
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
    let mut builder = http.request(method, &req.upstream.url);
    for (k, v) in &req.upstream.headers {
        if let Some(s) = v.as_str() {
            builder = builder.header(k, s);
        }
    }
    builder = builder.header("authorization", format!("Bearer {token}"));
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
        match lease.upstream_account_id.as_deref().filter(|s| !s.is_empty()) {
            Some(acct) => builder = builder.header("chatgpt-account-id", acct),
            // Better to say so than to send a request that can only 401 and be
            // misread as an expired credential. Re-importing the account (or
            // signing in through asale) is what fills this in.
            None => {
                tokens.report(&provider, &lease.account_id, &model, TaskOutcome::AuthFailed);
                send_error(
                    out,
                    &task_id,
                    "TOKEN_EXPIRED",
                    "codex account has no chatgpt-account-id; re-import the account",
                    false,
                );
                if let Some(r) = records {
                    r.record(&task_id, &provider, &lease.account_id, &model, &Usage::default(), "no_account_id").await;
                }
                return;
            }
        }
    }
    builder = builder.body(body);

    let resp = match builder.send().await {
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
            task = %task_id, provider = %provider, model = %model, status,
            "upstream rejected: {}", detail.chars().take(400).collect::<String>()
        );
        send_error(out, &task_id, code, &format!("upstream {status}"), retriable);
        if let Some(r) = records {
            r.record(&task_id, &provider, &lease.account_id, &model, &Usage::default(), &format!("upstream_{status}")).await;
        }
        return;
    }

    // stream_start
    let _ = out.send(Envelope::with_id(
        &task_id,
        protocol::T_STREAM_START,
        json!({"task_id": task_id, "status": status, "headers": {}}),
    ));

    // Stream body chunks; parse usage from SSE where possible.
    let mut stream = resp.bytes_stream();
    let mut seq: u64 = 0;
    let mut usage = Usage::default();
    let mut budget_hit = false;

    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(e) => {
                tokens.report(&provider, &lease.account_id, &model, TaskOutcome::ServerError);
                send_error(out, &task_id, "UPSTREAM_5XX", &format!("stream: {e}"), true);
                if let Some(r) = records {
                    r.record(&task_id, &provider, &lease.account_id, &model, &usage, "stream_error").await;
                }
                return;
            }
        };
        accumulate_usage(&mut usage, &bytes);
        // Budget guard: interrupt if output exceeds the granted budget.
        if usage.output_tokens > req.budget_tokens && req.budget_tokens > 0 {
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

    // stream_end with the best usage we have.
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
        TaskOutcome::Success { tokens_used: (usage.input_tokens + usage.output_tokens).max(0) as u64 },
    );

    // Local metering record for reconciliation (spec §5.2 step 5, §8).
    if let Some(r) = records {
        let status_label = if budget_hit { "budget" } else { "ok" };
        r.record(&task_id, &provider, &lease.account_id, &model, &usage, status_label).await;
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

/// Extract usage from provider SSE bodies (Claude/OpenAI/Responses/Gemini
/// shapes). Public so the consumer proxy can meter market/direct streams the
/// same way.
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
            // Claude message_delta.usage / message_start.message.usage
            if let Some(u) = v.get("usage") {
                if let Some(i) = u.get("input_tokens").and_then(|x| x.as_i64()) {
                    usage.input_tokens = i;
                }
                if let Some(o) = u.get("output_tokens").and_then(|x| x.as_i64()) {
                    usage.output_tokens = o;
                }
            }
            if let Some(m) = v.get("message").and_then(|m| m.get("usage")) {
                if let Some(i) = m.get("input_tokens").and_then(|x| x.as_i64()) {
                    usage.input_tokens = i;
                }
            }
            // OpenAI usage
            if let Some(u) = v.get("usage").filter(|u| u.get("prompt_tokens").is_some()) {
                usage.input_tokens = u.get("prompt_tokens").and_then(|x| x.as_i64()).unwrap_or(usage.input_tokens);
                usage.output_tokens = u.get("completion_tokens").and_then(|x| x.as_i64()).unwrap_or(usage.output_tokens);
            }
            // Responses API: the only usage frame is response.completed, which
            // nests it one level down. Without this, every codex stream would
            // settle as zero tokens.
            if let Some(u) = v.get("response").and_then(|r| r.get("usage")).filter(|u| !u.is_null()) {
                if let Some(i) = u.get("input_tokens").and_then(|x| x.as_i64()) {
                    usage.input_tokens = i;
                }
                if let Some(o) = u.get("output_tokens").and_then(|x| x.as_i64()) {
                    usage.output_tokens = o;
                }
            }
            // Gemini usageMetadata
            if let Some(u) = v.get("usageMetadata") {
                usage.input_tokens = u.get("promptTokenCount").and_then(|x| x.as_i64()).unwrap_or(usage.input_tokens);
                usage.output_tokens = u.get("candidatesTokenCount").and_then(|x| x.as_i64()).unwrap_or(usage.output_tokens);
            }
        }
    }
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

    /// The gateway key the tests pretend is pinned into the build.
    fn test_gateway_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[3u8; 32])
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
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), r, &tx, None, &test_verifier()).await;
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
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), r, &tx, None, &verifier).await;
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
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), r, &tx, None, &verifier).await;
        let frames = drain(&mut rx);
        assert_eq!(frames.first().unwrap().msg_type, protocol::T_STREAM_START);
        assert!(frames.iter().any(|f| f.msg_type == protocol::T_STREAM_END));
    }

    #[tokio::test]
    async fn no_token_reports_expired_and_records() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rows = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink { rows: rows.clone() };
        execute(&crate::http::plain(), &StaticToken(None), req("http://127.0.0.1:1/", "claude-x", 0), &tx, Some(&sink), &test_verifier()).await;
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
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), req(&url, "claude-sonnet", 0), &tx, Some(&sink), &test_verifier()).await;
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
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), req(&url, "claude", 100), &tx, None, &test_verifier()).await;
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
        execute(&crate::http::plain(), &tok, req(&url, "claude", 0), &tx, None, &test_verifier()).await;
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
        execute(&crate::http::plain(), &tok, req(&url, "claude", 0), &tx, None, &test_verifier()).await;
        let got = outcomes.lock().unwrap().clone();
        assert_eq!(got, vec![TaskOutcome::Success { tokens_used: 18 }]);
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
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), req(&url, "claude-opus-5", 0), &tx, None, &test_verifier())
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
            })
        }
    }

    fn codex_req(url: &str) -> HttpRequestPayload {
        let mut r = req(url, "gpt-5.1", 0);
        r.upstream.provider = "codex".into();
        r
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
        execute(&crate::http::plain(), &CodexToken(Some("acc-1")), codex_req(&url), &tx, None, &test_verifier()).await;
        let _ = server.await;

        let raw = seen.lock().unwrap().to_lowercase();
        assert!(raw.contains("chatgpt-account-id: acc-1"), "account id missing from the wire: {raw}");
        // Claude's OAuth requirements are Claude's; they must not follow along.
        assert!(!raw.contains("anthropic-beta"), "claude headers leaked onto codex: {raw}");
    }

    /// Without the id the request can only 401, and a 401 here is indistinguishable
    /// from a revoked login — so fail with a message that names the actual fix
    /// instead of letting the pool mislabel the account.
    #[tokio::test]
    async fn codex_without_an_account_id_fails_before_the_call() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Port 1 would fail loudly if the request were ever sent.
        execute(&crate::http::plain(), &CodexToken(None), codex_req("http://127.0.0.1:1/"), &tx, None, &test_verifier())
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
        execute(&crate::http::plain(), &StaticToken(Some("k".into())), req(&url, "claude", 0), &tx, None, &test_verifier()).await;
        let frames = drain(&mut rx);
        let e = frames.iter().find(|f| f.msg_type == protocol::T_ERROR).unwrap();
        assert_eq!(e.payload["code"], "UPSTREAM_4XX");
        assert_eq!(e.payload["retriable"], true); // 429 is retriable
    }
}
