//! Reproduce the market relay's upstream call and print what Anthropic really
//! answers, header variant by header variant.
//!
//!   cargo run --example relay_probe
//!
//! The relay builds its request server-side (`translator::claude::build_upstream`)
//! and the executor only adds `authorization`. This probe sends that exact shape,
//! then the same call with the OAuth beta flag and with a Claude Code system
//! prompt, so a 4xx can be attributed to a header rather than to quota.

use asale_client_core::store::LocalStore;
use asale_daemon::keychain;
use serde_json::{json, Value};

#[tokio::main]
async fn main() {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let store = LocalStore::open(&format!("{home}/.asale/asale.db")).await.expect("open store");
    let tools = store.list_tools().await.expect("list tools");
    let Some(t) = tools.iter().find(|t| t.provider == "claude" || t.provider == "claude_work") else {
        println!("no claude account connected");
        return;
    };
    let key = keychain::token_ref(&t.provider, &t.account_id);
    let token = keychain::get(&key).ok().flatten().expect("token in secret store");
    println!("account {} / {} — token {} chars\n", t.provider, t.account_id, token.len());

    let http = asale_client_core::http::upstream();
    let model = std::env::var("PROBE_MODEL").unwrap_or_else(|_| "claude-opus-5".into());
    let cc_system = "You are Claude Code, Anthropic's official CLI for Claude.";

    let base_body = json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        "stream": false,
    });
    let mut cc_body = base_body.clone();
    cc_body.as_object_mut().unwrap().insert("system".into(), json!(cc_system));
    // A caller's own system prompt (what codex/openai traffic carries) kept as a
    // second block behind the required Claude Code preamble.
    let mut cc_blocks_body = base_body.clone();
    cc_blocks_body.as_object_mut().unwrap().insert(
        "system".into(),
        json!([
            {"type": "text", "text": cc_system},
            {"type": "text", "text": "You are a helpful coding agent. Answer briefly."}
        ]),
    );
    // The caller's system prompt alone — what the relay sends today for a codex
    // request that carries `instructions`.
    let mut foreign_system_body = base_body.clone();
    foreign_system_body
        .as_object_mut()
        .unwrap()
        .insert("system".into(), json!("You are a helpful coding agent. Answer briefly."));

    // (label, extra headers, body)
    let cases: Vec<(&str, Vec<(&str, &str)>, &Value)> = vec![
        ("relay-as-is (no beta, no system)", vec![], &base_body),
        ("relay + oauth beta", vec![("anthropic-beta", "oauth-2025-04-20")], &base_body),
        ("claude-code system, NO beta", vec![], &cc_body),
        ("relay + oauth beta + claude-code system", vec![("anthropic-beta", "oauth-2025-04-20")], &cc_body),
        ("beta + [claude-code, caller] system blocks", vec![("anthropic-beta", "oauth-2025-04-20")], &cc_blocks_body),
        ("beta + caller-only system", vec![("anthropic-beta", "oauth-2025-04-20")], &foreign_system_body),
    ];

    for (label, extra, body) in cases {
        let mut req = http
            .post("https://api.anthropic.com/v1/messages")
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .header("user-agent", "claude-cli/1.0 (external, cli)")
            .header("authorization", format!("Bearer {token}"));
        for (k, v) in &extra {
            req = req.header(*k, *v);
        }
        match req.json(body).timeout(std::time::Duration::from_secs(30)).send().await {
            Ok(r) => {
                let status = r.status();
                let retry = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
                let text = r.text().await.unwrap_or_default();
                println!("[{label}]\n  HTTP {status}  retry-after={retry}\n  {}\n", &text.chars().take(600).collect::<String>());
            }
            Err(e) => println!("[{label}]\n  transport error: {e}\n"),
        }
    }
}
