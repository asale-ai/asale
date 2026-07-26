//! Check that provider endpoints are actually reachable from wherever the
//! daemon runs.
//!
//!   cargo run --example proxy_probe
//!
//! Prints the proxy the daemon would use, then calls each provider's OAuth
//! token endpoint with a deliberately invalid refresh token. A rejection naming
//! the *token* (`invalid_grant`) means the endpoint was reached and only the
//! credential was bad — which is the healthy result here. A 403 means the
//! connection itself was refused (region block) and no login will ever succeed
//! until a proxy is configured.
//!
//! Nothing real is sent: the dummy token cannot refresh anything, so this never
//! rotates or invalidates a stored credential.

const DUMMY: &str = "asale-proxy-probe-invalid-refresh-token";

#[tokio::main]
async fn main() {
    match asale_client_core::http::upstream_proxy() {
        Some(p) => println!("upstream proxy: {p}"),
        None => println!("upstream proxy: none (direct connection)"),
    }
    println!("override with {}=http://host:port (or =off to force direct)\n", asale_client_core::http::PROXY_ENV);

    for provider in ["claude", "codex", "gemini"] {
        let Some(adapter) = asale_daemon::publisher::adapter_for(provider) else { continue };
        match adapter.refresh(DUMMY).await {
            // Cannot happen with a dummy token, but report it rather than lie.
            Ok(_) => println!("{provider:12} UNEXPECTED: dummy token accepted"),
            Err(e) => {
                let msg = e.to_string();
                let verdict = if msg.contains("403") {
                    "BLOCKED  — endpoint refused the connection; configure a proxy"
                } else if msg.contains("error sending request") || msg.contains("dns") {
                    "UNREACHABLE — network/proxy error"
                } else {
                    "reachable — endpoint answered, rejecting the dummy token as expected"
                };
                println!("{provider:12} {verdict}\n             {msg}\n");
            }
        }
    }
}
