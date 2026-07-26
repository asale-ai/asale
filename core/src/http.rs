//! Outbound HTTP clients, split by destination.
//!
//! Provider endpoints (Anthropic / OpenAI / Google) are geo-restricted. From a
//! blocked region `api.anthropic.com` answers *every* request — including the
//! unauthenticated OAuth token endpoint — with
//! `403 {"error":{"type":"forbidden","message":"Request not allowed"}}`, which
//! surfaced as `refresh response missing access_token` in the refresh loop.
//!
//! Users in those regions run a local proxy, but nothing was routing us through
//! it: the daemon is normally launched from the desktop shell (Tauri/launchd),
//! which inherits none of the shell's `https_proxy`, and reqwest is built with
//! `default-features = false`, which drops the macOS system-proxy reader. Both
//! holes together meant every provider call dialled direct.
//!
//! Two clients come out of this, differing only in *who decides the proxy*:
//!
//!   - [`upstream`] — provider calls. Follows the user's saved [`ProxyPref`],
//!     falling back to the environment and then the OS.
//!   - [`plain`] — asale's own server and anything on this machine. Follows the
//!     environment/OS only: the proxy a user picks for providers must not
//!     silently redirect our control traffic, but it may still be the only way
//!     out of a restricted network, so we do not force it off either.
//!
//! Both exclude loopback unconditionally. Left to itself reqwest sends even
//! `127.0.0.1` through `http_proxy`, which is never what anyone means.

use std::sync::RwLock;
use std::time::Duration;

/// Explicit upstream proxy override, e.g. `http://127.0.0.1:7890`. The values
/// `off` / `none` / `direct` force a direct connection. Set in the environment,
/// it outranks the saved preference — a deployment escape hatch for when the UI
/// is unreachable.
pub const PROXY_ENV: &str = "ASALE_UPSTREAM_PROXY";

/// Env vars consulted for a proxy, in precedence order.
const PROXY_KEYS: [&str; 7] =
    [PROXY_ENV, "https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY", "http_proxy", "HTTP_PROXY"];

/// Hosts never sent through a proxy, whatever the environment says.
const LOCAL_NO_PROXY: &str = "localhost,127.0.0.1,::1";

const UA: &str = concat!("asale-client/", env!("CARGO_PKG_VERSION"));

/// How the user wants provider traffic routed. Persisted by the daemon and
/// mirrored here so every call site sees the change without a restart.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProxyPref {
    /// Follow the environment, then the OS proxy settings.
    #[default]
    Auto,
    /// Never proxy, even when the environment names one.
    Direct,
    /// Always use this proxy URL.
    Manual(String),
}

impl ProxyPref {
    /// Decode the persisted form: absent/empty → auto, `off` → direct, else a URL.
    pub fn from_setting(v: Option<&str>) -> ProxyPref {
        match v.map(str::trim).filter(|s| !s.is_empty()) {
            None => ProxyPref::Auto,
            Some(s) if is_off(s) => ProxyPref::Direct,
            Some(s) if s.eq_ignore_ascii_case("auto") => ProxyPref::Auto,
            Some(s) => ProxyPref::Manual(s.to_string()),
        }
    }

    /// Encode for the settings table (round-trips through `from_setting`).
    pub fn to_setting(&self) -> String {
        match self {
            ProxyPref::Auto => "auto".into(),
            ProxyPref::Direct => "off".into(),
            ProxyPref::Manual(u) => u.clone(),
        }
    }

    /// Stable tag for the UI's three-way selector.
    pub fn mode(&self) -> &'static str {
        match self {
            ProxyPref::Auto => "auto",
            ProxyPref::Direct => "off",
            ProxyPref::Manual(_) => "manual",
        }
    }
}

fn is_off(s: &str) -> bool {
    matches!(s.to_ascii_lowercase().as_str(), "off" | "none" | "direct")
}

static PREF: RwLock<ProxyPref> = RwLock::new(ProxyPref::Auto);
/// Cached OS lookup: `None` = not probed yet, `Some(v)` = probed, `v` = result.
static SYSTEM: RwLock<Option<Option<String>>> = RwLock::new(None);
/// Cached clients, each tagged with the proxy it was built for so a preference
/// change rebuilds instead of serving a stale route.
static UPSTREAM: RwLock<Option<(Option<String>, reqwest::Client)>> = RwLock::new(None);
static PLAIN: RwLock<Option<(Option<String>, reqwest::Client)>> = RwLock::new(None);

/// Apply a preference (the daemon calls this at startup and on every save).
/// Also drops the cached OS lookup, so switching back to auto re-probes.
pub fn set_preference(pref: ProxyPref) {
    *PREF.write().unwrap() = pref;
    *SYSTEM.write().unwrap() = None;
}

/// The preference currently in effect.
pub fn preference() -> ProxyPref {
    PREF.read().unwrap().clone()
}

/// True when `PROXY_ENV` is set, i.e. the environment is overriding whatever
/// the user picked in the UI. The UI shows this so the setting never looks
/// silently ignored.
pub fn env_override() -> Option<String> {
    let v = std::env::var(PROXY_ENV).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())?;
    Some(if is_off(&v) { "off".to_string() } else { v })
}

/// The proxy provider calls go through, or `None` for direct.
pub fn upstream_proxy() -> Option<String> {
    if let Some(v) = env_override() {
        return (!is_off(&v)).then_some(v);
    }
    match preference() {
        ProxyPref::Direct => None,
        ProxyPref::Manual(url) => Some(url),
        ProxyPref::Auto => ambient_proxy(),
    }
}

/// What [`ProxyPref::Auto`] would resolve to. Distinct from [`upstream_proxy`],
/// which answers for the preference actually in force — a caller testing "would
/// auto work?" must not be told about the `Direct` that is saved today.
pub fn auto_proxy() -> Option<String> {
    ambient_proxy()
}

/// The proxy asale's own traffic goes through: environment/OS only.
pub fn plain_proxy() -> Option<String> {
    ambient_proxy()
}

/// Environment first, then the OS. The OS step is what makes a GUI launch work:
/// the desktop shell starts the daemon with none of the shell's exports, so the
/// variables a user set in their terminal simply are not there.
fn ambient_proxy() -> Option<String> {
    for key in PROXY_KEYS {
        let Some(v) = std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else {
            continue;
        };
        if key == PROXY_ENV && is_off(&v) {
            return None;
        }
        return Some(v);
    }
    cached_system_proxy()
}

fn cached_system_proxy() -> Option<String> {
    if let Some(hit) = SYSTEM.read().unwrap().as_ref() {
        return hit.clone();
    }
    // Probed outside the lock: `scutil` is a subprocess and this runs on the
    // request path.
    let found = system_proxy();
    *SYSTEM.write().unwrap() = Some(found.clone());
    found
}

/// The shared client for provider (upstream) calls.
pub fn upstream() -> reqwest::Client {
    cached(&UPSTREAM, upstream_proxy())
}

/// The shared client for asale's own server and loopback services.
pub fn plain() -> reqwest::Client {
    cached(&PLAIN, plain_proxy())
}

fn cached(slot: &RwLock<Option<(Option<String>, reqwest::Client)>>, want: Option<String>) -> reqwest::Client {
    if let Some((have, client)) = slot.read().unwrap().as_ref() {
        if *have == want {
            return client.clone();
        }
    }
    let client = build(want.as_deref());
    *slot.write().unwrap() = Some((want, client.clone()));
    client
}

/// Build a client for `proxy` (exposed so a candidate proxy can be tested
/// before it is saved).
pub fn build(proxy: Option<&str>) -> reqwest::Client {
    // `.no_proxy()` first: left to itself reqwest re-reads `https_proxy` & co.
    // and would quietly proxy a connection we resolved as direct, making
    // "off" a lie. Resolution above is the authority, not reqwest.
    let mut b = reqwest::Client::builder().user_agent(UA).connect_timeout(Duration::from_secs(20)).no_proxy();
    if let Some(url) = proxy {
        match reqwest::Proxy::all(url) {
            Ok(p) => b = b.proxy(p.no_proxy(no_proxy())),
            // A bad proxy string must not take the daemon down: fall back to
            // direct and say so, since that is the state that then fails.
            Err(e) => tracing::warn!("ignoring invalid proxy {url:?}: {e}"),
        }
    }
    b.build().unwrap_or_else(|e| {
        tracing::warn!("http client build failed ({e}); falling back to defaults");
        reqwest::Client::new()
    })
}

/// Reject a proxy URL before it is saved, so a typo cannot silently strand the
/// daemon on a route nothing can reach.
pub fn validate(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("proxy address is empty".into());
    }
    let scheme = url.split("://").next().unwrap_or("");
    if !url.contains("://") || !matches!(scheme, "http" | "https" | "socks5" | "socks5h") {
        return Err("proxy address must start with http://, https:// or socks5://".into());
    }
    reqwest::Proxy::all(url).map(|_| ()).map_err(|e| e.to_string())
}

/// Loopback plus whatever the environment excludes.
fn no_proxy() -> Option<reqwest::NoProxy> {
    let mut list = LOCAL_NO_PROXY.to_string();
    if let Some(env) = std::env::var("no_proxy").or_else(|_| std::env::var("NO_PROXY")).ok().filter(|v| !v.is_empty())
    {
        list.push(',');
        list.push_str(&env);
    }
    reqwest::NoProxy::from_string(&list)
}

/// The OS-level HTTP(S) proxy, if one is enabled.
///
/// Read via `scutil`(1) rather than reqwest's `macos-system-configuration`
/// feature on purpose: that feature applies to *every* client in the process
/// and offers no way to exempt one.
#[cfg(target_os = "macos")]
fn system_proxy() -> Option<String> {
    let out = std::process::Command::new("scutil").arg("--proxy").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |k: &str| -> Option<String> {
        text.lines().find_map(|l| {
            let (name, value) = l.split_once(':')?;
            (name.trim() == k).then(|| value.trim().to_string())
        })
    };
    // HTTPS first: it is the one that carries our CONNECT tunnels.
    for (enable, host, port) in [("HTTPSEnable", "HTTPSProxy", "HTTPSPort"), ("HTTPEnable", "HTTPProxy", "HTTPPort")] {
        if field(enable).as_deref() != Some("1") {
            continue;
        }
        let (Some(host), Some(port)) = (field(host), field(port)) else { continue };
        if host.is_empty() || port.is_empty() {
            continue;
        }
        return Some(format!("http://{host}:{port}"));
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn system_proxy() -> Option<String> {
    // Windows/Linux: the env vars above are the convention; nothing else to read.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_env() {
        for k in PROXY_KEYS {
            std::env::remove_var(k);
        }
    }

    /// Env and the preference are process-global, so the cases share one test.
    #[test]
    fn resolution_order() {
        clear_env();
        set_preference(ProxyPref::Auto);
        // With nothing set the OS decides (possibly "none"); asserting a fixed
        // value here would depend on the dev machine.
        assert_eq!(upstream_proxy(), ambient_proxy(), "auto + no env → whatever the OS says");

        std::env::set_var("http_proxy", "http://127.0.0.1:7890");
        assert_eq!(upstream_proxy().as_deref(), Some("http://127.0.0.1:7890"));

        std::env::set_var("https_proxy", "http://127.0.0.1:1080");
        assert_eq!(upstream_proxy().as_deref(), Some("http://127.0.0.1:1080"), "https_proxy outranks http_proxy");

        // A saved preference beats the ambient environment...
        set_preference(ProxyPref::Manual("http://127.0.0.1:2222".into()));
        assert_eq!(upstream_proxy().as_deref(), Some("http://127.0.0.1:2222"));
        set_preference(ProxyPref::Direct);
        assert_eq!(upstream_proxy(), None, "an explicit 'direct' is not overridden by env proxies");

        // ...but asale's own traffic keeps following the environment,
        assert_eq!(plain_proxy().as_deref(), Some("http://127.0.0.1:1080"), "the UI setting is provider-only");
        // ...and "what would auto do?" stays answerable while Direct is saved,
        // so the Settings page can test a mode before committing to it.
        assert_eq!(auto_proxy().as_deref(), Some("http://127.0.0.1:1080"));

        // ...and the env escape hatch beats the saved preference either way.
        std::env::set_var(PROXY_ENV, "http://127.0.0.1:3333");
        assert_eq!(upstream_proxy().as_deref(), Some("http://127.0.0.1:3333"));
        std::env::set_var(PROXY_ENV, "off");
        set_preference(ProxyPref::Manual("http://127.0.0.1:2222".into()));
        assert_eq!(upstream_proxy(), None, "env opt-out wins over a saved proxy");

        clear_env();
        set_preference(ProxyPref::Auto);
    }

    #[test]
    fn preference_round_trips_through_the_settings_table() {
        for pref in
            [ProxyPref::Auto, ProxyPref::Direct, ProxyPref::Manual("http://127.0.0.1:7890".into())]
        {
            assert_eq!(ProxyPref::from_setting(Some(&pref.to_setting())), pref);
        }
        assert_eq!(ProxyPref::from_setting(None), ProxyPref::Auto, "unset → auto");
        assert_eq!(ProxyPref::from_setting(Some("  ")), ProxyPref::Auto, "blank → auto");
        assert_eq!(ProxyPref::from_setting(Some("none")), ProxyPref::Direct);
    }

    #[test]
    fn validate_rejects_what_would_strand_the_daemon() {
        assert!(validate("http://127.0.0.1:7890").is_ok());
        assert!(validate("socks5://127.0.0.1:1080").is_ok());
        assert!(validate("").is_err(), "empty");
        assert!(validate("127.0.0.1:7890").is_err(), "no scheme");
        assert!(validate("ftp://127.0.0.1:21").is_err(), "unsupported scheme");
    }

    #[test]
    fn a_client_is_always_produced() {
        // Must degrade to direct rather than panic the refresh loop.
        let _ = build(Some("not a url"));
        let _ = build(None);
    }
}
