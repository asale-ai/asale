//! asale-daemon — the real client backend as a standalone service.
//!
//! Owns everything native and server-facing: the encrypted secret store, the
//! consumer local proxy (:9787), the publisher WS session, token refresh, usage
//! aggregation, CLI credential scan/import, Claude Code config switching, and
//! the asale server REST calls. Exposes it all over a local HTTP API
//! (`POST /rpc/{cmd}`) and serves the embedded web UI, so:
//!
//!   - the Tauri desktop shell is a thin webview over this daemon,
//!   - Chrome on the same machine can open the app directly (http://127.0.0.1:9700),
//!   - a headless Linux box can run `asaled --bind 0.0.0.0:9700` and be used
//!     remotely from any browser (B/S), gated by the daemon token.

pub mod auth_store;
pub mod cli_scan;
pub mod codex_catalog;
pub mod commands;
pub mod keychain;
pub mod logging;
pub mod oauth;
pub mod proc_scan;
pub mod proxy;
pub mod publisher;
pub mod quota_poll;
pub mod rpc;
pub mod selfcheck;
pub mod state;
pub mod tool_config;
pub mod usage_scan;
pub mod version_gate;

use state::AppState;
use std::net::SocketAddr;
use std::sync::Arc;

/// Test-only helper for the modules that repoint the app data dir.
#[cfg(test)]
pub(crate) mod testenv {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    /// `$ASALE_DATA_DIR` is a process-global env var, so a per-module mutex is
    /// not enough — tests in *different* modules would still run concurrently
    /// and delete each other's temp dir. Every test that repoints it takes this
    /// one lock.
    static DATA_DIR_LOCK: Mutex<()> = Mutex::new(());
    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// Hold the same lock without repointing anything — for the tests that
    /// assert what `data_dir()` resolves to when `$ASALE_DATA_DIR` is *unset*.
    /// They have to clear it, which is the same interference this lock exists
    /// to prevent.
    pub fn lock_data_dir() -> std::sync::MutexGuard<'static, ()> {
        DATA_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` with `$ASALE_DATA_DIR` pointed at a fresh empty directory,
    /// restoring the previous value (and removing the directory) afterwards.
    pub fn with_temp_data_dir<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = DATA_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("ASALE_DATA_DIR").ok();
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("asale-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("ASALE_DATA_DIR", &tmp);
        let out = f();
        match prev {
            Some(p) => std::env::set_var("ASALE_DATA_DIR", p),
            None => std::env::remove_var("ASALE_DATA_DIR"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
        out
    }
}

/// A running daemon: shared state + where it listens + the remote-access token.
pub struct StartedDaemon {
    pub state: Arc<AppState>,
    pub addr: SocketAddr,
    pub token: String,
}

pub const DEFAULT_BIND: &str = "127.0.0.1:9700";

/// The address this process actually ended up listening on, once it has.
///
/// Not the same thing as `resolve_bind`: a caller can ask for port 0, and the
/// Settings page has to be able to say where the app *is* — including whether
/// it is reachable from other machines, which is the difference between "only
/// you can open this" and "anyone with the token can".
static BOUND: std::sync::OnceLock<SocketAddr> = std::sync::OnceLock::new();

pub fn bound_addr() -> Option<SocketAddr> {
    BOUND.get().copied()
}

/// Resolve the bind address: explicit arg > $ASALE_BIND > 127.0.0.1:9700.
pub fn resolve_bind(cli: Option<&str>) -> anyhow::Result<SocketAddr> {
    let raw = cli
        .map(String::from)
        .or_else(|| std::env::var("ASALE_BIND").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| DEFAULT_BIND.to_string());
    raw.parse::<SocketAddr>().map_err(|e| anyhow::anyhow!("invalid bind address {raw}: {e}"))
}

/// Load (or create on first run) the daemon access token used by non-loopback
/// clients. Lives at `<data_dir>/daemon.token`, mode 0600.
pub fn load_or_create_token() -> anyhow::Result<String> {
    let dir = std::path::PathBuf::from(state::data_dir());
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("daemon.token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    use rand::RngCore;
    let mut raw = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    use base64::Engine;
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    std::fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(token)
}

/// Build the state and launch every long-lived component on the current tokio
/// runtime: consumer proxy, token-refresh loop, usage-aggregation loop, and the
/// RPC/UI HTTP server. Returns once the HTTP listener is bound.
pub async fn start(bind: SocketAddr) -> anyhow::Result<StartedDaemon> {
    let app_state = Arc::new(AppState::new().await?);
    let token = load_or_create_token()?;

    // Apply the saved proxy preference before anything reaches for a provider.
    if let Err(e) = commands::load_proxy_pref(&app_state).await {
        tracing::warn!("could not load the saved proxy setting: {e}");
    }

    // Say up front how provider traffic leaves this machine. A GUI launch
    // inherits no shell exports, so "none" here is the first sign that token
    // refresh and direct routing are about to be region-blocked.
    match asale_client_core::http::upstream_proxy() {
        Some(p) => tracing::info!("upstream proxy: {p}"),
        None => tracing::info!(
            "upstream proxy: none (direct) — set {} if provider endpoints are blocked here",
            asale_client_core::http::PROXY_ENV
        ),
    }

    // Consumer local proxy (spec §6) on the configured port. The proxy forwards
    // model calls to the gateway HTTP service, NOT the web REST API.
    {
        let proxy_state = proxy::ProxyState {
            server_api_base: app_state.cfg.gateway_api_base.clone(),
            asale_key: app_state.asale_key.clone(),
            http: asale_client_core::http::plain(),
            store: app_state.store.clone(),
            pool: app_state.pool.clone(),
            app: Some(app_state.clone()),
            remint_lock: Default::default(),
        };
        let proxy_port = app_state.cfg.proxy_port;
        tokio::spawn(async move {
            let router = proxy::router(proxy_state);
            // Retry the bind briefly: on a restart the previous instance may
            // still hold the port while shutting down.
            let mut listener = None;
            for attempt in 0..20 {
                match tokio::net::TcpListener::bind(("127.0.0.1", proxy_port)).await {
                    Ok(l) => {
                        listener = Some(l);
                        break;
                    }
                    Err(e) if attempt < 19 => {
                        tracing::warn!("proxy bind failed ({e}), retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    Err(e) => tracing::error!("proxy bind failed, giving up: {e}"),
                }
            }
            if let Some(listener) = listener {
                tracing::info!("consumer proxy on 127.0.0.1:{proxy_port}");
                let _ = axum::serve(listener, router).await;
            }
        });
    }

    // Preload the cached asale key into the proxy (dropping one that was minted
    // against a different deployment — see `commands::wallet::cached_key`).
    if let Err(e) = commands::load_api_key(&app_state).await {
        tracing::warn!("could not load the cached asale api key: {e}");
    }

    // Carry an older build's Claude-only subscription over to the per-tool buy
    // switch before the proxy serves anything (it gates on that switch).
    if let Err(e) = commands::migrate_legacy_subscription(&app_state).await {
        tracing::warn!("legacy subscription migration failed: {e}");
    }

    // A tool whose buy switch is on must have a config that points at the
    // proxy. Anything could have rewritten it while the daemon was down (another
    // switcher, an editor, an upgrade), and the user should not have to notice
    // that, let alone repair it by hand — so put it back before the proxy serves
    // its first request.
    commands::reconcile_configs(&app_state).await;

    // Which models this device may advertise comes from the server's tradable
    // catalog, not from a list baked into the binary. Pull it before the pool
    // is rebuilt below so the sell page shows the real set on the first paint;
    // offline, the cached (or built-in) set carries until the recovery loop
    // gets through.
    if let Err(e) = publisher::refresh_sellable_catalog(&app_state.store, &app_state.cfg.server_api_base).await {
        tracing::warn!("sellable catalog refresh failed at startup: {e}");
    }
    // And what the market pays for those models, before the first rebuild: the
    // recovery loop would get to it within a minute, but that minute is the one
    // the user spends looking at a Sell page whose prices are all blank.
    if let Err(e) = publisher::refresh_market_prices(&app_state.store, &app_state.cfg.server_api_base).await {
        tracing::warn!("market price refresh failed at startup: {e}");
    }
    publisher::rebuild_pool(&app_state.store, &app_state.pool).await;

    // Is this build still allowed to trade at all? Asked on its own slow clock
    // rather than waited for here — the answer gates a dialog, not the daemon,
    // and a machine that is offline must still start (see `version_gate`).
    version_gate::spawn_loop(app_state.cfg.server_api_base.clone());

    // Token-refresh loop runs for the daemon lifetime so subscription tokens
    // stay fresh even before the first publish (spec §3.4).
    {
        let handle = publisher::spawn_refresh_loop(app_state.store.clone(), app_state.pool.clone());
        *app_state.refresh_task.write().await = Some(handle);
    }

    // Lane bookkeeping (spec §4.5): persist operator-clearable pauses and push
    // every availability change to the market, plus a timer that wakes exactly
    // when a cooling lane or a daily cap is due back.
    {
        if let Some(rx) = app_state.take_lane_rx() {
            publisher::spawn_lane_loop(app_state.store.clone(), app_state.publisher.clone(), rx);
        }
        publisher::spawn_recovery_loop(
            app_state.store.clone(),
            app_state.pool.clone(),
            app_state.publisher.clone(),
            app_state.cfg.server_api_base.clone(),
        );
    }

    // The subscription windows themselves, read from the providers that publish
    // them. Without this the sell gate only ever sees its own local estimate,
    // which is blind to the operator's own use of the subscription and pinned
    // to the lowest paid tier whenever the login carried no plan.
    quota_poll::spawn_quota_loop(app_state.clone());

    // Auto-import local CLI credentials at startup (spec §3.3): the machine's
    // installed Claude Code / Codex / gemini-cli accounts join the sell pool
    // without the user having to run a scan from the UI.
    {
        let st = app_state.clone();
        tokio::spawn(async move {
            match commands::import_cli_all(&st).await {
                Ok(v) => {
                    let n = v["imported"].as_array().map(|a| a.len()).unwrap_or(0);
                    tracing::info!("cli auto-import: {n} account(s) imported");
                }
                Err(e) => tracing::warn!("cli auto-import failed: {e}"),
            }
        });
    }

    // Selling is a persisted decision, not a per-process one: a device the user
    // switched on stays on the market across restarts, crashes and outages. The
    // supervisor is what makes that true — it also covers the cases the session
    // loop cannot recover from by itself (not signed in yet at boot, a kick
    // whose cause has since been cleared).
    {
        let st = app_state.clone();
        tokio::spawn(async move {
            // Pull the policy first so the very first declaration already
            // carries the right floors.
            if let Err(e) = commands::publish_policy_get(&st).await {
                tracing::debug!("publisher policy pull skipped: {e}");
            }
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            let mut complained = false;
            loop {
                tick.tick().await;
                // Selling through a custom endpoint is a platform-operator
                // capability now, and it did not used to be — so an ordinary
                // seller may still have one connected from when it was open to
                // everyone. The gateway refuses those lanes on every
                // declaration; this is what stops them being declared at all,
                // and what takes them off the sell page rather than leaving an
                // account there that can never earn. Costs a local read on a
                // device with no custom endpoint, which is nearly all of them.
                commands::enforce_custom_endpoint_policy(&st).await;
                if !commands::publish_wanted(&st).await {
                    continue;
                }
                // Idempotent while a live session exists.
                match commands::set_publishing(&st, true).await {
                    Ok(_) => complained = false,
                    // Retried every minute, so this must not shout every
                    // minute: the common cause is simply not signed in yet.
                    Err(e) if complained => tracing::debug!("publishing still not resumable: {e}"),
                    Err(e) => {
                        complained = true;
                        tracing::warn!("resuming publishing failed (will keep retrying): {e}");
                    }
                }
            }
        });
    }

    // Usage snapshot loop: fold new ledger rows + local AI-CLI logs every 60s.
    {
        let store = app_state.store.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = store.aggregate_usage().await {
                    tracing::warn!("usage aggregate error: {e}");
                }
                if let Err(e) = usage_scan::scan_claude_logs(&store).await {
                    tracing::warn!("usage scan error: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
    }

    // RPC + web UI server.
    let ctx = rpc::Ctx { state: app_state.clone(), token: Arc::new(token.clone()) };
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    let _ = BOUND.set(addr);
    tokio::spawn(async move {
        let app = rpc::router(ctx).into_make_service_with_connect_info::<SocketAddr>();
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("rpc server exited: {e}");
        }
    });
    tracing::info!("asaled listening on http://{addr} (every /rpc call needs X-Asale-Token)");

    Ok(StartedDaemon { state: app_state, addr, token })
}

/// True when something already answers /healthz at `addr` (used by the desktop
/// shell to reuse an externally started daemon instead of double-binding).
pub async fn probe(addr: &str) -> bool {
    let url = format!("http://{addr}/healthz");
    match asale_client_core::http::plain().get(&url).timeout(std::time::Duration::from_millis(800)).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}
pub mod session;
