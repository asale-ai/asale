//! Why a Codex sale settles as zero tokens.
//!
//!   cargo run --example codex_usage_probe
//!
//! `executor::accumulate_usage` is handed one raw `bytes_stream()` chunk at a
//! time and parses each in isolation — there is no buffer carrying a partial
//! `data:` line into the next chunk. Claude survives that because its usage
//! frame (`message_delta`) is ~100 bytes and arrives whole. The Responses API
//! keeps usage in `response.completed`, whose payload embeds the entire response
//! object, so the question is simply whether that frame ever lands inside a
//! single chunk.
//!
//! This streams a real Codex response and prints, per chunk, how the SSE was cut
//! up, then compares the usage the executor's own parser recovers against the
//! usage present in the reassembled stream.

use asale_client_core::store::LocalStore;
use asale_protocol::payload::Usage;
use asale_daemon::keychain;
use futures_util::StreamExt;
use serde_json::{json, Value};

const URL: &str = "https://chatgpt.com/backend-api/codex/responses";

#[tokio::main]
async fn main() {
    let db = format!("{}/asale.db", asale_daemon::state::data_dir());
    let store = LocalStore::open(&db).await.expect("open store");
    let tools = store.list_tools().await.expect("list tools");
    let Some(tool) = tools.into_iter().find(|t| t.provider == "codex") else {
        println!("no codex account connected in {db}");
        return;
    };
    let Some(token) = keychain::get(&tool.keychain_ref).ok().flatten() else {
        println!("no token in secret store for {}", tool.keychain_ref);
        return;
    };
    let account_id = asale_client_core::executor::chatgpt_account_id(&token).unwrap_or_default();
    let model = std::env::var("PROBE_MODEL").unwrap_or_else(|_| "gpt-5.6-sol".into());
    println!("account {} · model {model}", tool.account_id);

    let body = json!({
        "model": model,
        "input": [{ "type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "reply with exactly: hello codex"}] }],
        "stream": true,
        "store": false,
        "instructions": "You are a helpful assistant.",
        "reasoning": {"effort": "low"},
    });

    let resp = asale_client_core::http::upstream()
        .post(URL)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("openai-beta", "responses=experimental")
        .header("originator", "codex_cli_rs")
        .header("user-agent", "codex_cli_rs/0.146.0")
        .header("session_id", uuid::Uuid::new_v4().to_string())
        .header("authorization", format!("Bearer {token}"))
        .header("chatgpt-account-id", account_id)
        .json(&body)
        .timeout(std::time::Duration::from_secs(90))
        .send()
        .await
        .expect("send");
    println!("HTTP {}\n", resp.status());

    // Side by side: the old per-chunk call with no carry-over, and the scanner
    // the executor uses now.
    let mut per_chunk = Usage::default();
    let mut scan = asale_client_core::executor::UsageScanner::new();
    let mut whole = Vec::new();
    let mut chunks = 0usize;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.expect("chunk");
        chunks += 1;
        asale_client_core::executor::accumulate_usage(&mut per_chunk, &bytes);
        scan.push(&bytes);
        whole.extend_from_slice(&bytes);
    }
    let scanned = scan.flush();

    // And what the same parser recovers once the stream is whole again.
    let mut reassembled = Usage::default();
    asale_client_core::executor::accumulate_usage(&mut reassembled, &whole);

    let text = String::from_utf8_lossy(&whole);
    let completed = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .find(|v| v.get("type").and_then(Value::as_str) == Some("response.completed"));

    println!("{chunks} chunks, {} bytes total", whole.len());
    match &completed {
        Some(v) => {
            let frame = serde_json::to_string(v).unwrap_or_default();
            println!("response.completed frame: {} bytes", frame.len());
            println!("  usage in it: {}", v.pointer("/response/usage").cloned().unwrap_or(Value::Null));
        }
        None => println!("no response.completed frame in the stream at all"),
    }
    println!(
        "\nraw per-chunk parse (the old behaviour):   in={} out={}",
        per_chunk.input_tokens, per_chunk.output_tokens
    );
    println!(
        "UsageScanner (what the executor reports): in={} out={}",
        scanned.input_tokens, scanned.output_tokens
    );
    println!(
        "same parser on the reassembled stream:    in={} out={}",
        reassembled.input_tokens, reassembled.output_tokens
    );
    if per_chunk.output_tokens == 0 && reassembled.output_tokens > 0 {
        println!("\n→ the usage frame was split across chunks — which the old code lost outright");
    }
    if scanned == reassembled && scanned.output_tokens > 0 {
        println!("→ the scanner recovers it in full, so this sale settles and pays");
    }
}
