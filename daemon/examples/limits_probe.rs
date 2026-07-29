//! Real end-to-end probe of the Limits page's live-data path: read the connected
//! Claude account's OAuth token from the encrypted secret store, call the real
//! Anthropic usage endpoint, and print what the Limits panel would render.
//!
//!   cargo run --example limits_probe
//!
//! Reads the same `asale.db` + `secrets.enc` the app uses (`$ASALE_DATA_DIR`,
//! `~/.asale` by default); it
//! never writes anything.

use asale_client_core::store::LocalStore;
use asale_daemon::keychain;
use serde_json::Value;

#[tokio::main]
async fn main() {
    // Follow $ASALE_DATA_DIR like the daemon does, so a probe run next to
    // `pnpm dev:app` reads that app's store instead of the release build's.
    let db = format!("{}/asale.db", asale_daemon::state::data_dir());
    let store = LocalStore::open(&db).await.expect("open store");
    let tools = store.list_tools().await.expect("list tools");
    println!("connected accounts: {}", tools.len());
    match keychain::keys() {
        Ok(ks) => println!("secret-store keys: {ks:?}"),
        Err(e) => println!("secret-store read error: {e}"),
    }

    // Same proxy-aware client the daemon uses, so the probe reproduces its
    // reachability rather than the shell's.
    let http = asale_client_core::http::upstream();
    for t in &tools {
        if t.provider != "claude" && t.provider != "claude_work" {
            println!("- {} / {} (skipped: not Claude-family)", t.provider, t.account_id);
            continue;
        }
        let key = keychain::token_ref(&t.provider, &t.account_id);
        let token = match keychain::get(&key).ok().flatten() {
            Some(tok) => tok,
            None => {
                println!("- {} / {}: NO TOKEN in secret store (key={key})", t.provider, t.account_id);
                continue;
            }
        };
        println!(
            "- {} / {}: token loaded ({} chars), calling oauth/usage…",
            t.provider, t.account_id, token.len()
        );

        let resp = http
            .get("https://api.anthropic.com/api/oauth/usage")
            .header("Authorization", format!("Bearer {token}"))
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;

        match resp {
            Ok(r) => {
                let status = r.status();
                let body: Value = r.json().await.unwrap_or(Value::Null);
                println!("    HTTP {status}");
                if status.is_success() {
                    // Exactly the rows the Limits panel would draw, labels included.
                    for w in asale_daemon::commands::normalize_claude_windows(&body) {
                        println!(
                            "    {:<10} used={:>6.2}%  resets_at={}",
                            w["label"].as_str().unwrap_or("?"),
                            w["used_percent"].as_f64().unwrap_or(0.0),
                            w["reset_at"].as_str().unwrap_or("—"),
                        );
                    }
                } else {
                    println!("    body: {}", serde_json::to_string(&body).unwrap_or_default());
                    println!("    → the page falls back to the local estimate, labelled 「估算」.");
                    if status.as_u16() == 403 {
                        println!("      A 403 here is the region block, not a bad token: set an");
                        println!("      upstream proxy in Settings — asaled launched from the desktop");
                        println!("      shell inherits none of your shell's https_proxy.");
                    }
                }
            }
            Err(e) => println!("    request error: {e} → the page falls back to the local estimate."),
        }
    }
}
