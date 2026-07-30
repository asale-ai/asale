//! Real probe of the Codex publisher path, end to end against the live upstream.
//!
//!   cargo run --example codex_probe
//!
//! The relay logs an upstream rejection but only ever tells the consumer
//! "upstream 400", so the sentence that explains a failed sale is invisible from
//! the market side. This prints it, and checks the three things that have to
//! line up for a Codex sale to work:
//!
//!   1. which slugs the ChatGPT account is entitled to ask for
//!      (`/backend-api/codex/models`, the answer that made `gpt-5.1` fail);
//!   2. which lanes this device would therefore declare (`rebuild_pool`);
//!   3. that the body the relay builds is accepted by a slug from (1).
//!
//! Reads the same `asale.db` + `secrets.enc` the app uses; the only thing it
//! writes is the entitlement cache the daemon would write anyway.

use asale_client_core::pool::{AccountPool, Strategy};
use asale_client_core::store::LocalStore;
use asale_daemon::keychain;
use serde_json::{json, Value};
use std::sync::Mutex as StdMutex;

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
    let plan = asale_client_core::cli_import::jwt_claims(&token)
        .and_then(|c| c.pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type").cloned())
        .unwrap_or(Value::Null);
    println!("account {} · plan {plan} · chatgpt-account-id {account_id}", tool.account_id);

    // 1. What this account may actually ask for.
    let entitled = asale_client_core::discovery::codex_servable_models(&token, &account_id)
        .await
        .expect("codex models");
    println!("\nentitled slugs: {entitled:?}");

    // 2. What this device would declare, catalog ∩ entitlement.
    let pool = StdMutex::new(AccountPool::new(Strategy::RoundRobin));
    asale_daemon::publisher::rebuild_pool(&store, &pool).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let lanes: Vec<String> = pool
        .lock()
        .unwrap()
        .lane_views(now)
        .into_iter()
        .filter(|l| l.provider == "codex")
        .map(|l| format!("{} [{}]", l.model, l.status))
        .collect();
    println!("declared codex lanes: {lanes:?}");

    // 3. Does the relay's body work on an entitled slug?
    let Some(slug) = entitled.first() else {
        println!("nothing entitled — nothing to send");
        return;
    };
    let user = json!([{ "type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "只回复两个字：你好"}] }]);
    for (name, model) in [("entitled", slug.as_str()), ("what the market matched before", "gpt-5.1")] {
        let body = json!({
            "model": model,
            "input": user,
            "stream": true,
            "store": false,
            "instructions": "You are a helpful assistant.",
            "reasoning": {"effort": "low"},
            "tools": [{"type": "function", "name": "grep", "description": "search",
                       "parameters": {"type": "object", "properties": {}}}],
            "tool_choice": "auto"
        });
        probe(&token, &account_id, &format!("{name}: {model}"), &body).await;
    }
}

async fn probe(token: &str, account_id: &str, name: &str, body: &Value) {
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
        .json(body)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            // The answer itself, not the lifecycle framing around it.
            let said: String = text
                .lines()
                .filter_map(|l| l.strip_prefix("data: "))
                .filter_map(|d| serde_json::from_str::<Value>(d).ok())
                .filter(|v| v.get("type").and_then(Value::as_str) == Some("response.output_text.delta"))
                .filter_map(|v| v.get("delta").and_then(Value::as_str).map(String::from))
                .collect();
            let head: String = text.chars().take(200).collect();
            println!("\n[{name}] {status}\n  said: {said:?}\n  raw: {}", head.replace('\n', " "));
        }
        Err(e) => println!("\n[{name}] transport error: {e}"),
    }
}
