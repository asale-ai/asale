//! Check whether the asale SERVER has real records/wallet for the logged-in
//! user (the Usage/Limits pages currently read only the local client ledger).
//!
//!   cargo run --example server_probe

use asale_daemon::keychain;
use serde_json::Value;

#[tokio::main]
async fn main() {
    let base = std::env::var("ASALE_SERVER_API").unwrap_or_else(|_| "http://127.0.0.1:9090".into());
    let token = match keychain::get("access_token").ok().flatten() {
        Some(t) => t,
        None => { println!("not logged in (no access_token)"); return; }
    };
    println!("server: {base}  token: {} chars\n", token.len());
    let http = reqwest::Client::new();

    for path in ["/api/v1/me/wallet", "/api/v1/me/records?role=provider&page=1", "/api/v1/me/records?role=consumer&page=1"] {
        let r = http.get(format!("{base}{path}"))
            .header("authorization", format!("Bearer {token}"))
            .timeout(std::time::Duration::from_secs(10))
            .send().await;
        match r {
            Ok(resp) => {
                let status = resp.status();
                let body: Value = resp.json().await.unwrap_or(Value::Null);
                let count = body.get("records").and_then(|v| v.as_array()).map(|a| a.len());
                print!("GET {path}\n    HTTP {status}");
                if let Some(n) = count { print!("  records={n}"); }
                println!();
                // Show a compact preview so we can see if real data exists.
                let preview = serde_json::to_string(&body).unwrap_or_default();
                println!("    {}", &preview[..preview.len().min(300)]);
            }
            Err(e) => println!("GET {path}\n    error: {e}"),
        }
    }
}
