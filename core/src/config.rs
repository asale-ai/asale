//! Client configuration (persisted in settings / env in the daemon layer).

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub server_api_base: String,  // web REST API — https://api.asale.xxx (:9090 dev)
    pub gateway_api_base: String, // consumer gateway HTTP — https://gw.asale.xxx (:9081 dev)
    pub gateway_ws_url: String,   // wss://gw.asale.xxx/v1/ws
    pub proxy_port: u16,          // local consumer proxy (spec §6.1)
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            server_api_base: env(
                "ASALE_SERVER_API",
                option_env!("ASALE_SERVER_API"),
                "http://127.0.0.1:9090",
            ),
            gateway_api_base: env(
                "ASALE_GATEWAY_API",
                option_env!("ASALE_GATEWAY_API"),
                "http://127.0.0.1:9081",
            ),
            gateway_ws_url: env(
                "ASALE_GATEWAY_WS",
                option_env!("ASALE_GATEWAY_WS"),
                "ws://127.0.0.1:9082/v1/ws",
            ),
            proxy_port: env("ASALE_PROXY_PORT", option_env!("ASALE_PROXY_PORT"), "9787")
                .parse()
                .unwrap_or(9787),
        }
    }
}

impl ClientConfig {
    /// Refuse to talk to a remote host without TLS.
    ///
    /// The signed handshake authenticates the *connection*, once. It does not
    /// key the channel, so everything after it — the relayed prompt bodies, the
    /// upstream responses, the device token in the upgrade headers — is carried
    /// entirely by whatever the transport provides. On plain `ws://` that is
    /// nothing: an on-path attacker reads all of it and can inject frames into
    /// an already-authenticated session.
    ///
    /// Loopback is exempt because there is no network to be on-path of, which
    /// is what keeps `cargo run` against a local gateway working.
    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, url) in [
            ("ASALE_SERVER_API", &self.server_api_base),
            ("ASALE_GATEWAY_API", &self.gateway_api_base),
            ("ASALE_GATEWAY_WS", &self.gateway_ws_url),
        ] {
            check_transport(name, url)?;
        }
        Ok(())
    }
}

/// Host part of a `scheme://host[:port]/...` URL.
fn host_of(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    // Strip userinfo and port. IPv6 literals are bracketed.
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    match authority.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map(|(h, _)| h).unwrap_or(rest),
        None => authority.split_once(':').map(|(h, _)| h).unwrap_or(authority),
    }
}

/// Whether this host is the local machine, and so unreachable from the network.
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        // A name this build cannot resolve to an IP is treated as remote:
        // erring toward "needs TLS" is the safe direction.
        Err(_) => false,
    }
}

fn check_transport(name: &str, url: &str) -> anyhow::Result<()> {
    let scheme = if url.starts_with("ws://") {
        "ws://"
    } else if url.starts_with("http://") {
        "http://"
    } else {
        return Ok(());
    };
    let host = host_of(url);
    if is_loopback_host(host) {
        return Ok(());
    }
    anyhow::bail!(
        "{name} points at a remote host over an unencrypted connection ({url}). \
         Use https:// / wss:// — the handshake signature authenticates the connection \
         but does not encrypt it, so on plain {scheme} the relayed prompts, responses \
         and device token travel in clear text."
    )
}

/// Run-time environment > value baked in at build time > dev default.
///
/// A packaged desktop client is launched from Finder/Explorer/a `.desktop`
/// entry, with none of the shell environment the developer had — so a release
/// build has to carry its endpoints, the same way it carries the pinned quota
/// key (see `security::pinned_quota_pubkey`). The packaging step supplies them:
///
/// ```text
/// ASALE_SERVER_API=https://api.asale.ai \
/// ASALE_GATEWAY_API=https://gw.asale.ai \
/// ASALE_GATEWAY_WS=wss://gw.asale.ai/v1/ws  pnpm tauri build
/// ```
///
/// The run-time variable still wins, so a shipped binary can be pointed at a
/// staging gateway for one session without a rebuild. Nothing is trusted here
/// that was not already trusted: an attacker who can set this process's
/// environment owns the process anyway. `validate()` below still refuses any of
/// these values in plaintext against a non-loopback host, baked in or not.
fn env(k: &str, baked: Option<&str>, def: &str) -> String {
    // An explicitly-empty variable means "unset" at every level, so each step
    // falls through to the next rather than shipping an empty base URL.
    let pick = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    std::env::var(k)
        .ok()
        .and_then(|v| pick(&v))
        .or_else(|| baked.and_then(pick))
        .unwrap_or_else(|| def.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(server: &str, gw: &str, ws: &str) -> ClientConfig {
        ClientConfig {
            server_api_base: server.into(),
            gateway_api_base: gw.into(),
            gateway_ws_url: ws.into(),
            proxy_port: 9787,
        }
    }

    #[test]
    fn loopback_may_stay_plaintext_so_local_development_works() {
        assert!(cfg("http://127.0.0.1:9090", "http://localhost:9081", "ws://[::1]:9082/v1/ws")
            .validate()
            .is_ok());
    }

    #[test]
    fn a_remote_host_over_plaintext_is_refused() {
        let err = cfg("https://api.asale.app", "https://gw.asale.app", "ws://gw.asale.app/v1/ws")
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("ASALE_GATEWAY_WS"), "should name the offending var: {err}");

        assert!(cfg("http://198.51.100.7:9090", "https://gw.asale.app", "wss://gw.asale.app/v1/ws")
            .validate()
            .is_err());
    }

    #[test]
    fn tls_endpoints_pass() {
        assert!(cfg("https://api.asale.app", "https://gw.asale.app", "wss://gw.asale.app/v1/ws")
            .validate()
            .is_ok());
    }

    #[test]
    fn host_parsing_survives_ports_userinfo_and_ipv6() {
        assert_eq!(host_of("http://127.0.0.1:9090/x"), "127.0.0.1");
        assert_eq!(host_of("ws://[::1]:9082/v1/ws"), "::1");
        assert_eq!(host_of("https://user:pw@api.asale.app/x?y=1"), "api.asale.app");
        assert_eq!(host_of("https://api.asale.app"), "api.asale.app");
    }
}
