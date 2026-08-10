//! Buy your own lane on purpose, to find out whether it works.
//!
//! # Why this cannot be a local check
//!
//! A seller's page can show that the switch is on, the token is fresh, the lane
//! is declared and the device is online, and every one of those can be true
//! while nothing the seller owns is able to answer a buyer. The gap is the part
//! neither side can see alone: the gateway has to have this lane in its index,
//! matching has to consider it servable, the relay frame has to arrive over a
//! socket that is really open, the quota grant has to verify against the key
//! pinned into *this* build, the pool has to hand out a credential, and the
//! vendor has to answer it. "Online, earning nothing" is the symptom of any of
//! those, and they are indistinguishable from the sell page.
//!
//! So the only honest test is a real purchase. This is one: the same consumer
//! API key, the same gateway host, the same catalog and price checks, the same
//! preauthorization, the same `T_HTTP_REQUEST` frame, the same executor, the
//! same metering and settlement afterwards. Nothing here is a mock and nothing
//! is bypassed — a green result means a buyer's request would have worked,
//! because one just did.
//!
//! The single difference is who serves it: the request carries
//! [`H_TARGET_DEVICE`](asale_protocol::frame::H_TARGET_DEVICE), which the
//! gateway honours only for a device on the caller's own account, so matching
//! is narrowed to this machine instead of choosing across the market. It is
//! narrowed and not exempted — a lane out of window, out of slots or cooling
//! off is refused here exactly as it would be for a stranger, because a test
//! that could pass where a buyer would fail is worse than no test.
//!
//! # What it costs
//!
//! Real money, in both directions: the account pays the market price for the
//! answer and earns the sale of it, netting the platform's fee, and the tokens
//! come off the subscription's own window. That is not a side effect to be
//! engineered away — it is the same trade a buyer makes, which is what makes
//! the result mean anything. It is kept to one short prompt and
//! [`MAX_TOKENS`] of reply, and the UI says so before the button is pressed.

use serde_json::{json, Value};
use std::time::Instant;

use crate::state::AppState;
use super::{R, now_secs};

/// The prompt. Short, and phrased so a correct answer is recognisable at a
/// glance rather than needing to be read — the point of showing the reply is to
/// prove a model produced it, not to learn anything from it.
const PROMPT: &str = "Reply with exactly one word: ok";

/// Reply ceiling.
///
/// Not the smallest number that could work. A reasoning model spends output
/// tokens thinking before it writes anything, so a ceiling of 8 buys a
/// truncated reply with no text in it — an empty answer that looks like a
/// failure and is not one. This is small enough to be pocket change and large
/// enough that the models we trade actually finish a word.
const MAX_TOKENS: i64 = 64;

/// How long to wait for the whole round trip before calling it hung.
///
/// Generous on purpose: this path includes a cold upstream connection and, on a
/// reasoning model, real thinking time. The failure it is here to catch is a
/// relay that never answers at all, and that one does not resolve itself in
/// thirty seconds any more than in ninety.
const TIMEOUT_SECS: u64 = 90;

/// The protocols a test request can be written in.
///
/// This is a *buyer's* choice, not a property of the seller: the gateway
/// translates between dialects, so an OpenAI-shaped request is legitimately
/// served by a Claude subscription. Offering the choice is what makes the test
/// able to answer "can the tool I care about buy this", since a buyer running
/// Codex speaks Responses and one running Claude Code speaks Anthropic, and the
/// translation in between is itself something that can be broken.
struct Wire {
    id: &'static str,
    /// Gateway path. `{model}` is substituted — Gemini names the model in the
    /// URL rather than in the body.
    path: &'static str,
}

const WIRES: &[Wire] = &[
    Wire { id: "openai", path: "/v1/chat/completions" },
    Wire { id: "claude", path: "/v1/messages" },
    Wire { id: "gemini", path: "/v1beta/models/{model}:generateContent" },
    Wire { id: "responses", path: "/v1/responses" },
];

/// The request body a buyer speaking `wire` would send.
///
/// Deliberately the plainest form of each dialect — no tools, no system prompt,
/// no streaming. Everything left out is something that could fail for its own
/// reasons and confuse the answer to the one question being asked.
fn body_for(wire: &str, model: &str) -> Value {
    match wire {
        "claude" => json!({
            "model": model,
            "max_tokens": MAX_TOKENS,
            "messages": [{"role": "user", "content": PROMPT}],
        }),
        "gemini" => json!({
            "contents": [{"role": "user", "parts": [{"text": PROMPT}]}],
            "generationConfig": {"maxOutputTokens": MAX_TOKENS},
        }),
        "responses" => json!({
            "model": model,
            "input": PROMPT,
            "max_output_tokens": MAX_TOKENS,
        }),
        // OpenAI chat completions, and the fallback: it is the dialect every
        // reseller implements, so it is the safest thing to send at a lane
        // whose wire we somehow do not recognise.
        _ => json!({
            "model": model,
            "max_tokens": MAX_TOKENS,
            "messages": [{"role": "user", "content": PROMPT}],
            "stream": false,
        }),
    }
}

/// The assistant's words, wherever this dialect keeps them.
///
/// Best-effort by design. An empty string is not treated as a failure anywhere
/// below: a model that spent its whole ceiling on reasoning really did serve
/// the request, and the tokens it reports are the proof. This is here to show
/// the user something recognisable when there is something to show.
fn reply_text(wire: &str, v: &Value) -> String {
    let text = match wire {
        "claude" => v["content"]
            .as_array()
            .map(|parts| collect_text(parts.iter().filter(|p| p["type"] == "text"), "text"))
            .unwrap_or_default(),
        "gemini" => v["candidates"][0]["content"]["parts"]
            .as_array()
            .map(|parts| collect_text(parts.iter(), "text"))
            .unwrap_or_default(),
        "responses" => v["output"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i["content"].as_array())
                    .flat_map(|c| c.iter().filter(|p| p["type"] == "output_text"))
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default(),
        _ => v["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string(),
    };
    // A model that ignores "one word" must not push the rest of the result
    // panel off the screen.
    text.trim().chars().take(200).collect()
}

fn collect_text<'a>(parts: impl Iterator<Item = &'a Value>, field: &str) -> String {
    parts.filter_map(|p| p[field].as_str()).collect::<Vec<_>>().join("")
}

/// Tokens as this dialect reports them. `(input, output)`, zero when absent.
fn usage_of(wire: &str, v: &Value) -> (i64, i64) {
    let n = |x: &Value| x.as_i64().unwrap_or(0);
    match wire {
        "claude" => (n(&v["usage"]["input_tokens"]), n(&v["usage"]["output_tokens"])),
        "gemini" => (
            n(&v["usageMetadata"]["promptTokenCount"]),
            n(&v["usageMetadata"]["candidatesTokenCount"]),
        ),
        "responses" => (n(&v["usage"]["input_tokens"]), n(&v["usage"]["output_tokens"])),
        _ => (
            n(&v["usage"]["prompt_tokens"]),
            n(&v["usage"]["completion_tokens"]),
        ),
    }
}

/// What a gateway response means for the test, decided from its status line and
/// two headers before a byte of the body is read.
///
/// A function of its own because "did this work" is the entire product here and
/// it is *not* the status code. Two of these four outcomes wear a 200, and both
/// of them mean the seller learned nothing about their own lane — which is the
/// one answer this feature must never give quietly.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// A status code the gateway meant as a refusal.
    Refused,
    /// A 200 the gateway meant as a refusal, rendered as an assistant turn.
    Notice,
    /// A 200 that really is an answer — from somebody else's lane.
    Unpinned,
    /// A 200 from this device.
    Served,
}

fn verdict(status: u16, notice: &str, served_self: bool) -> Verdict {
    if !(200..300).contains(&status) {
        Verdict::Refused
    } else if !notice.trim().is_empty() {
        Verdict::Notice
    } else if !served_self {
        Verdict::Unpinned
    } else {
        Verdict::Served
    }
}

/// Who the gateway says served this, straight off the response headers.
///
/// Worth surfacing even on a success: `self` proves the pin was honoured, and
/// `upstream` is what catches a lane serving from a different subscription than
/// the row the button was pressed on — one device can hold several accounts of
/// the same provider, and which one the pool leases is its own decision.
fn provenance(h: &reqwest::header::HeaderMap) -> Value {
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    json!({
        "upstream": get("x-asale-upstream"),
        "model": get("x-asale-model"),
        "task": get("x-asale-task"),
        "self": get("x-asale-self") == "1",
    })
}

/// Run one real purchase against this device's own lane.
///
/// `provider` and `account_id` name the row the button was pressed on; they are
/// checked here and reported back, but they do not steer the request. Supply is
/// declared per `(device, model)` and the seller-side pool picks the account
/// when the task arrives, so the honest thing to do is say which account
/// actually served (see [`provenance`]) rather than to promise one in advance.
pub async fn test_supply(
    state: &AppState,
    provider: String,
    account_id: String,
    model: String,
    wire: String,
) -> R<Value> {
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err(crate::cmd_err!("errors.probe.noModel", "choose a model to test"));
    }
    let wire = WIRES
        .iter()
        .find(|w| w.id == wire.trim())
        .unwrap_or(&WIRES[0]);

    // Refuse before spending anything on a lane that cannot possibly be
    // reached. These two are not the market's answer to the test — they are the
    // reason the test would have no answer, and the sell page can already say
    // what to do about them.
    crate::publisher::rebuild_pool(&state.store, &state.pool).await;
    let lane = {
        let pool = state.pool.lock().map_err(|_| "pool lock poisoned".to_string())?;
        pool.lane_views(now_secs())
            .into_iter()
            .find(|l| l.provider == provider && l.account_id == account_id && l.model == model)
    };
    let Some(lane) = lane else {
        return Err(crate::cmd_err!(
            "errors.probe.noLane",
            format!("`{model}` is not one of this account's models"),
            model = model.clone()
        ));
    };
    if !lane.sell_enabled {
        return Err(crate::cmd_err!(
            "errors.probe.sellOff",
            "turn selling on for this account first"
        ));
    }

    let key = super::buy::ensure_consumer_key(state).await?;
    let base = state.cfg.gateway_api_base.trim_end_matches('/');
    let url = format!("{base}{}", wire.path.replace("{model}", &model));
    let body = body_for(wire.id, &model);

    let started = Instant::now();
    let resp = send(state, &url, &body, &key).await;
    // A key the gateway does not know is not a verdict on this lane. It is the
    // same fault the proxy self-heals on every buy, and reporting it as "your
    // selling is broken" would send the user to look at their subscription.
    let resp = match resp {
        Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => {
            super::buy::mint_consumer_key(state).await?;
            let key = state.asale_key.read().await.clone().unwrap_or(key);
            send(state, &url, &body, &key).await
        }
        other => other,
    };
    let elapsed_ms = started.elapsed().as_millis() as i64;

    // Transport failures never reached the gateway, so there is no status and
    // no provenance — only the reason the socket did not work.
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            return Ok(json!({
                "ok": false,
                "stage": "transport",
                "elapsed_ms": elapsed_ms,
                "model": model,
                "wire": wire.id,
                "error": { "message": e.to_string() },
            }));
        }
    };

    let status = resp.status();
    let prov = provenance(resp.headers());
    // Marks a 200 that is a refusal rather than a model's answer — see
    // `gateway::notice::H_NOTICE` on the server, and `proxy::forward_market`,
    // which reads the same header for the same reason.
    let notice = resp
        .headers()
        .get("x-asale-notice")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    let payload: Value = resp.json().await.unwrap_or(Value::Null);

    match verdict(status.as_u16(), &notice, prov["self"].as_bool().unwrap_or(false)) {
        // A status code the gateway meant as a refusal. Its envelope carries a
        // translation key and its values; the frontend renders them in the
        // user's own language, so they must not be flattened to the English
        // sentence on the way through.
        Verdict::Refused => {
            let e = super::server_client::server_error(&payload, "the gateway refused the request");
            return Ok(json!({
                "ok": false,
                "stage": "gateway",
                "status": status.as_u16(),
                "elapsed_ms": elapsed_ms,
                "model": model,
                "wire": wire.id,
                "provenance": prov,
                // The same `{error, key, params}` envelope a failed command
                // would have thrown, so the frontend decodes it with the one
                // helper it already has rather than a second shape to keep in
                // step.
                "error": e.to_json(),
            }));
        }
        // A refusal wearing an answer's clothes.
        //
        // For asale's own client the gateway renders "nobody is selling this",
        // "top up", "upgrade the app" as an assistant turn the buyer reads
        // inside their AI session rather than as a status code their tool would
        // print raw. That is right for someone mid-conversation and wrong for
        // this caller: scored on the status line alone, a lane that is not on
        // the market reads as a completed sale with the explanation sitting
        // unread in the body. The prose is already in the user's language, so
        // it is carried through as the message rather than rebuilt from a key.
        Verdict::Notice => {
            tracing::info!(%model, notice, "the platform refused the test as a notice");
            return Ok(json!({
                "ok": false,
                "stage": "gateway",
                "status": status.as_u16(),
                "elapsed_ms": elapsed_ms,
                "model": model,
                "wire": wire.id,
                "notice": notice,
                "provenance": prov,
                "error": { "error": reply_text(wire.id, &payload) },
            }));
        }
        // An answer, from somebody else's lane.
        //
        // A platform older than the pin ignores the header rather than
        // rejecting it, and then this is an ordinary buy: matching picks
        // whichever seller it likes, a stranger's lane serves it perfectly
        // well, and the seller who pressed the button is told their selling
        // works on the strength of a subscription that is not theirs. That is
        // the one wrong answer this feature can give, and it is worse than any
        // failure it could report.
        Verdict::Unpinned => {
            tracing::warn!(
                %model,
                "the platform served this test from another seller — the target-device pin was not honoured"
            );
            return Ok(json!({
                "ok": false,
                "stage": "unpinned",
                "status": status.as_u16(),
                "elapsed_ms": elapsed_ms,
                "model": model,
                "wire": wire.id,
                "provenance": prov,
            }));
        }
        Verdict::Served => {}
    }

    let (in_tokens, out_tokens) = usage_of(wire.id, &payload);
    // A real purchase belongs in this machine's own ledger like any other. The
    // `c_` id is the proxy's convention for a locally-minted consume row, so a
    // test looks like what it is — something this account bought — rather than
    // like a gap between the local total and the server's.
    let _ = state
        .store
        .insert_consume_record(
            &format!("c_{}", uuid::Uuid::new_v4().simple()),
            &model,
            in_tokens,
            out_tokens,
            0,
            "ok",
        )
        .await;

    tracing::info!(
        %model, wire = wire.id, elapsed_ms,
        served_by = prov["upstream"].as_str().unwrap_or(""),
        "supply self-test succeeded"
    );

    Ok(json!({
        "ok": true,
        "stage": "served",
        "status": status.as_u16(),
        "elapsed_ms": elapsed_ms,
        "model": model,
        "wire": wire.id,
        "provenance": prov,
        "reply": reply_text(wire.id, &payload),
        "in_tokens": in_tokens,
        "out_tokens": out_tokens,
    }))
}

/// The buy request, built the way the proxy builds one — plus the pin.
///
/// Same client, same bearer, same `accept-language`, so a refusal arrives in
/// the language the app is set to. What is deliberately *not* here is anything
/// marking this as a test: the gateway must treat it as ordinary demand, or the
/// path being measured is not the path buyers use.
async fn send(
    state: &AppState,
    url: &str,
    body: &Value,
    key: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req = asale_client_core::http::plain()
        .post(url)
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .header(asale_protocol::frame::H_TARGET_DEVICE, &state.device_id);
    if let Some(lang) = state.store.get_setting("language").await.ok().flatten() {
        if !lang.trim().is_empty() {
            req = req.header("accept-language", lang);
        }
    }
    req.json(body).send().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is OpenAI chat, and an unknown id lands on it rather than
    /// failing: the picker's value comes from a saved preference that may
    /// outlive the name it holds.
    #[test]
    fn the_first_wire_is_openai_and_catches_anything_unrecognised() {
        assert_eq!(WIRES[0].id, "openai");
        let body = body_for("something-else", "m");
        assert!(body["messages"].is_array());
        assert_eq!(body["stream"], false);
    }

    /// Gemini names the model in the path; every other dialect names it in the
    /// body. Getting this backwards produces a 404 from the gateway that reads
    /// like a missing model.
    #[test]
    fn the_model_is_named_where_each_dialect_expects_it() {
        for w in WIRES {
            let path = w.path.replace("{model}", "claude-fable-5");
            let body = body_for(w.id, "claude-fable-5");
            assert!(!path.contains("{model}"), "{} left a placeholder", w.id);
            if w.id == "gemini" {
                assert!(path.contains("claude-fable-5"));
                assert!(body.get("model").is_none(), "gemini takes no model field");
            } else {
                assert_eq!(body["model"], "claude-fable-5", "{}", w.id);
            }
        }
    }

    /// Every dialect must have its reply read out of the right place. A silent
    /// miss here shows the user an empty answer for a request that worked.
    #[test]
    fn a_reply_is_found_in_every_dialects_own_shape() {
        let cases = [
            ("openai", json!({"choices": [{"message": {"content": "ok"}}]})),
            ("claude", json!({"content": [{"type": "text", "text": "ok"}]})),
            ("gemini", json!({"candidates": [{"content": {"parts": [{"text": "ok"}]}}]})),
            (
                "responses",
                json!({"output": [{"content": [{"type": "output_text", "text": "ok"}]}]}),
            ),
        ];
        for (wire, v) in cases {
            assert_eq!(reply_text(wire, &v), "ok", "{wire}");
            // An answer this code cannot parse must read as empty, never panic:
            // the verdict is the request having been served, not the parsing.
            assert_eq!(reply_text(wire, &json!({})), "", "{wire} on an empty body");
        }
    }

    /// Thinking-only replies are the case this exists for: no text, real
    /// tokens. Reporting them as a failure would tell a seller their working
    /// lane is broken.
    #[test]
    fn a_reply_with_no_text_still_carries_its_usage() {
        let v = json!({"content": [], "usage": {"input_tokens": 12, "output_tokens": 64}});
        assert_eq!(reply_text("claude", &v), "");
        assert_eq!(usage_of("claude", &v), (12, 64));
    }

    #[test]
    fn usage_is_read_from_each_dialects_own_field_names() {
        assert_eq!(
            usage_of("openai", &json!({"usage": {"prompt_tokens": 5, "completion_tokens": 7}})),
            (5, 7)
        );
        assert_eq!(
            usage_of(
                "gemini",
                &json!({"usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 7}})
            ),
            (5, 7)
        );
        assert_eq!(usage_of("responses", &json!({})), (0, 0), "absent is zero, not a panic");
    }

    /// A model that ignores the instruction must not be able to push the rest
    /// of the result off the screen.
    #[test]
    fn a_long_reply_is_cut_down_to_something_renderable() {
        let long = "x".repeat(5000);
        let v = json!({"choices": [{"message": {"content": long}}]});
        assert_eq!(reply_text("openai", &v).chars().count(), 200);
    }

    /// The verdict is not the status code, and this is the test that says so.
    ///
    /// Found the hard way against a live gateway: with the lane priced off the
    /// market, the platform answered `200` carrying "nobody is selling this" as
    /// an assistant turn — the shape it uses so a buyer mid-conversation reads a
    /// sentence instead of a stack trace. Scored on the status line, that is a
    /// completed sale.
    #[test]
    fn a_refusal_dressed_as_a_200_is_not_a_sale() {
        assert_eq!(verdict(200, "no_supply", false), Verdict::Notice);
        // Even when the pin *was* honoured: a notice is a refusal whatever the
        // provenance says, and it is checked before the answer is believed.
        assert_eq!(verdict(200, "no_supply", true), Verdict::Notice);
        assert_eq!(verdict(200, "insufficient_balance", true), Verdict::Notice);
        assert_eq!(verdict(200, "client_upgrade_required", true), Verdict::Notice);
        // An absent header arrives as the empty string, and whitespace is the
        // same thing spelled by a proxy that rewrote it.
        assert_eq!(verdict(200, "", true), Verdict::Served);
        assert_eq!(verdict(200, "   ", true), Verdict::Served);
    }

    /// The one answer this feature must never give: a stranger's healthy lane
    /// reported as proof that yours works.
    #[test]
    fn an_answer_from_another_seller_is_a_failure_not_a_pass() {
        assert_eq!(verdict(200, "", false), Verdict::Unpinned);
        assert_eq!(verdict(201, "", false), Verdict::Unpinned);
    }

    #[test]
    fn a_status_code_refusal_is_still_a_refusal() {
        for s in [400u16, 402, 403, 429, 503] {
            assert_eq!(verdict(s, "", true), Verdict::Refused, "{s}");
        }
        // A status refusal outranks everything else it could be confused with.
        assert_eq!(verdict(503, "no_supply", false), Verdict::Refused);
    }

    /// A refused test carries the market's own words, in the one envelope the
    /// frontend knows how to decode (`toDaemonError` reads `error`/`key`/
    /// `params`, and nothing else). This is the contract that broke once:
    /// spelling the sentence into a `message` field instead put
    /// `[object Object]` on screen in place of the reason.
    #[test]
    fn a_refusal_reaches_the_frontend_in_the_shape_it_decodes() {
        // What the gateway answers a request nobody can serve with.
        let gateway = json!({
            "error": {
                "message": "no supply for model claude-fable-5",
                "key": "errors.market.noSupply",
                "params": { "model": "claude-fable-5" },
            }
        });
        let e = super::super::server_client::server_error(&gateway, "fallback");
        let envelope = e.to_json();
        assert_eq!(envelope["error"], "no supply for model claude-fable-5");
        assert_eq!(envelope["key"], "errors.market.noSupply");
        assert_eq!(envelope["params"]["model"], "claude-fable-5");
    }

    #[test]
    fn provenance_survives_headers_that_are_not_there() {
        let p = provenance(&reqwest::header::HeaderMap::new());
        assert_eq!(p["upstream"], "");
        assert_eq!(p["self"], false);
    }
}
