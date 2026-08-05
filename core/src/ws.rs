//! wsrelay client (spec §9). Manages the publisher session: signed handshake,
//! supply declaration, heartbeat, inbound `http_request` dispatch → executor,
//! and — the core reliability guarantee — exponential-backoff reconnection with
//! supply re-declaration and `control` (kick/throttle) handling.

use crate::executor::{self, RecordSink, TokenProvider};
use crate::protocol::{self, ControlPayload, Envelope, HttpRequestPayload};
use crate::security::{DeviceIdentity, QuotaVerifier};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// Live connection state, surfaced to the UI (spec §2.2 events / §9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Offline,
    Connecting,
    Reconnecting,
    Online,
    Throttled,
    /// Server kicked this device; the loop stops and won't auto-reconnect.
    Kicked,
}

impl ConnState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConnState::Offline => "offline",
            ConnState::Connecting => "connecting",
            ConnState::Reconnecting => "reconnecting",
            ConnState::Online => "online",
            ConnState::Throttled => "throttled",
            ConnState::Kicked => "kicked",
        }
    }
}

/// Connection parameters resolved fresh for each (re)connect so a rotated
/// `device_token` is picked up automatically.
pub struct WsConfig {
    pub gateway_ws_url: String,
    pub device_id: String,
    pub device_token: String,
}

/// Supplies the per-attempt connection config (registers/refreshes the device
/// token against the server). Implemented by the Tauri layer.
#[async_trait]
pub trait ConfigSource: Send + Sync {
    async fn ws_config(&self) -> anyhow::Result<WsConfig>;
}

/// Produces the current supply declaration (JSON array of `SupplyItem`), based
/// on imported accounts and their measured quota. Implemented by the Tauri layer.
#[async_trait]
pub trait SupplySource: Send + Sync {
    async fn declare_items(&self) -> serde_json::Value;
}

/// Notified when the gateway takes one of this device's lanes out of rotation.
///
/// The client normally notices trouble first and pauses the lane itself; this
/// fires when the gateway's own backstop trips instead, so the app can show the
/// operator the same "paused, resume when fixed" state rather than leaving them
/// to wonder why one model stopped earning.
pub trait LaneControl: Send + Sync {
    fn on_lane_pause(&self, model: &str, reason: &str, requires_user: bool);
}

/// Everything the publisher session needs beyond the config.
pub struct PublisherDeps {
    pub identity: Arc<DeviceIdentity>,
    pub tokens: Arc<dyn TokenProvider>,
    pub supply: Arc<dyn SupplySource>,
    pub records: Option<Arc<dyn RecordSink>>,
    pub lanes: Option<Arc<dyn LaneControl>>,
    /// Verifier for the gateway's quota grants, built from the key pinned into
    /// this build. Resolved once, before any socket is opened — see
    /// [`PublisherDeps::with_pinned_quota_key`].
    pub quota: Arc<QuotaVerifier>,
    /// The pinned key in base64, kept so `hello.ack` can be checked against it.
    pub quota_pubkey: String,
}

impl PublisherDeps {
    /// Fill in `quota`/`quota_pubkey` from the key this build pins.
    ///
    /// Fails when nothing is pinned, and that failure is what keeps the device
    /// off the market: publishing without the ability to verify a grant means
    /// spending the user's own subscription on whatever any peer claiming to be
    /// the gateway asks for.
    pub fn with_pinned_quota_key(
        identity: Arc<DeviceIdentity>,
        tokens: Arc<dyn TokenProvider>,
        supply: Arc<dyn SupplySource>,
        records: Option<Arc<dyn RecordSink>>,
        lanes: Option<Arc<dyn LaneControl>>,
    ) -> anyhow::Result<PublisherDeps> {
        let quota = Arc::new(crate::security::pinned_quota_verifier()?);
        // Unwrap is sound only after the line above succeeded: a verifier
        // exists exactly when a key was pinned.
        let quota_pubkey = crate::security::pinned_quota_pubkey().unwrap_or_default();
        Ok(PublisherDeps { identity, tokens, supply, records, lanes, quota, quota_pubkey })
    }
}

/// A pending "this lane is fixed" signal: a counter (so identical requests
/// still register as changes) and the model, empty meaning every lane.
type ResumeRequest = (u64, String);

/// A running publisher. Drop or call `stop` to disconnect gracefully.
pub struct PublisherHandle {
    shutdown_tx: watch::Sender<bool>,
    state_rx: watch::Receiver<ConnState>,
    nudge_tx: watch::Sender<u64>,
    resume_tx: watch::Sender<ResumeRequest>,
}

impl PublisherHandle {
    /// Current connection state.
    pub fn state(&self) -> ConnState {
        *self.state_rx.borrow()
    }

    /// A receiver to observe state transitions.
    pub fn state_receiver(&self) -> watch::Receiver<ConnState> {
        self.state_rx.clone()
    }

    /// Re-declare supply now instead of waiting for the next periodic tick.
    ///
    /// Everything that changes what this device is willing to sell — an
    /// account's sell switch, a daily cap, a price floor, an exhausted quota —
    /// has to reach the market immediately. On the periodic cadence alone the
    /// gateway would keep dispatching against withdrawn capacity for up to a
    /// full interval.
    pub fn nudge(&self) {
        self.nudge_tx.send_modify(|n| *n += 1);
    }

    /// Tell the gateway one of this device's lanes is serviceable again, then
    /// re-declare.
    ///
    /// A lane the gateway quarantined after repeated failures does *not* come
    /// back on a plain re-declaration — that is the point of a quarantine, or a
    /// broken publisher would rejoin every minute and cost another consumer a
    /// failover each time. This frame is the acknowledgement that a person
    /// looked at it. An empty `model` resumes every lane of the device.
    pub fn resume(&self, model: &str) {
        self.resume_tx.send_modify(|r| {
            r.0 += 1;
            r.1 = model.to_string();
        });
    }

    /// Signal a graceful shutdown; the loop closes the socket and stops.
    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

impl Drop for PublisherHandle {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Compute the next backoff delay: exponential (double), capped, with jitter.
/// Jitter is subtractive (up to 20% off) so the result never exceeds `cap` —
/// avoids a thundering herd when a gateway restarts.
pub fn next_backoff(current: Duration, cap: Duration) -> Duration {
    let doubled = current.saturating_mul(2).min(cap);
    let millis = doubled.as_millis() as u64;
    let jitter = fastrand_jitter(millis / 5); // 0..=20% of the delay
    Duration::from_millis(millis.saturating_sub(jitter))
}

/// Next supply sequence number for this session.
fn next_seq(counter: &std::sync::atomic::AtomicU64) -> u64 {
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// Small dependency-free jitter using system nanos.
fn fastrand_jitter(range: u64) -> u64 {
    if range == 0 {
        return 0;
    }
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() as u64;
    n % (range + 1)
}

const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Heartbeat cadence (spec §9.2). The gateway expires presence and supply on a
/// 90s clock, so this is also what keeps the device in the index.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Periodic full supply re-declaration.
///
/// A fallback, not the mechanism: real changes arrive through `nudge()`. It
/// stays comfortably inside the gateway's presence TTL and inside the analytics
/// rollup's staleness window, so a device that is genuinely serving never
/// flickers out of either.
const SUPPLY_REFRESH: Duration = Duration::from_secs(60);

/// Force a reconnect after this long with no inbound frame at all.
///
/// The gateway pings every 15s and acks every heartbeat, so silence this long
/// means the link is gone. Waiting for the socket to report it can take many
/// minutes on a half-open connection (closed laptop, dropped VPN) — the whole
/// time this client believes it is online and selling, while the gateway has
/// long since expired its supply.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Spawn the reconnecting publisher loop. Returns a handle immediately; the
/// session runs on the current tokio runtime.
pub fn spawn_publisher(cfg_src: Arc<dyn ConfigSource>, deps: PublisherDeps) -> PublisherHandle {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (state_tx, state_rx) = watch::channel(ConnState::Connecting);
    let (nudge_tx, nudge_rx) = watch::channel(0u64);
    let (resume_tx, resume_rx) = watch::channel((0u64, String::new()));
    let handle = PublisherHandle { shutdown_tx, state_rx, nudge_tx, resume_tx };

    tokio::spawn(async move {
        run_loop(cfg_src, deps, shutdown_rx, state_tx, nudge_rx, resume_rx).await;
    });

    handle
}

async fn run_loop(
    cfg_src: Arc<dyn ConfigSource>,
    deps: PublisherDeps,
    mut shutdown_rx: watch::Receiver<bool>,
    state_tx: watch::Sender<ConnState>,
    nudge_rx: watch::Receiver<u64>,
    resume_rx: watch::Receiver<ResumeRequest>,
) {
    let mut backoff = BACKOFF_START;
    let mut first = true;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        let _ = state_tx.send(if first { ConnState::Connecting } else { ConnState::Reconnecting });

        let cfg = match cfg_src.ws_config().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("ws config failed: {e}");
                if sleep_or_shutdown(&mut shutdown_rx, backoff).await {
                    break;
                }
                backoff = next_backoff(backoff, BACKOFF_CAP);
                continue;
            }
        };

        match run_session(&cfg, &deps, &mut shutdown_rx, &state_tx, &nudge_rx, &resume_rx).await {
            SessionOutcome::Shutdown => break,
            SessionOutcome::Kicked => {
                // Terminal: kick disables auto-reconnect and stays visible.
                let _ = state_tx.send(ConnState::Kicked);
                return;
            }
            SessionOutcome::Migrate => {
                // No sleep and no backoff growth: this was a planned handover,
                // not a fault, and the replacement node is already accepting.
                let _ = state_tx.send(ConnState::Reconnecting);
                backoff = BACKOFF_START;
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            SessionOutcome::Closed { was_online } => {
                if was_online {
                    backoff = BACKOFF_START; // healthy session → reset backoff.
                }
                if sleep_or_shutdown(&mut shutdown_rx, backoff).await {
                    break;
                }
                backoff = next_backoff(backoff, BACKOFF_CAP);
            }
        }
        first = false;
    }

    let _ = state_tx.send(ConnState::Offline);
}

enum SessionOutcome {
    Shutdown,
    Kicked,
    /// The node asked us to move. Reconnect immediately: waiting out a backoff
    /// would take this device off the market for no reason, and the whole point
    /// of the notice is that another node is already there to take it.
    Migrate,
    Closed { was_online: bool },
}

/// Await `dur`, returning true if a shutdown was requested meanwhile.
async fn sleep_or_shutdown(shutdown_rx: &mut watch::Receiver<bool>, dur: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => *shutdown_rx.borrow(),
        _ = shutdown_rx.changed() => *shutdown_rx.borrow(),
    }
}

/// Dig the required version out of a 426 body.
///
/// The gateway sends `{"error":{"key":…,"params":{"min":"0.4.0",…}}}`. Falling
/// back to an empty string is fine — the UI then says "an update is required"
/// without naming a version, which is still the actionable half of the message.
fn min_version_from_body(body: Option<&[u8]>) -> String {
    body.and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
        .and_then(|v| v["error"]["params"]["min"].as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The gateway this client intends to reach, as it will appear in the signed
/// handshake: the host of the WS URL, without port or path.
///
/// The gateway compares it against its own configured identity, so a signature
/// produced for one gateway cannot be forwarded to another and reused there.
pub fn gateway_audience(ws_url: &str) -> String {
    let after_scheme = ws_url.split_once("://").map(|(_, r)| r).unwrap_or(ws_url);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    match authority.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map(|(h, _)| h).unwrap_or(rest).to_string(),
        None => authority.split_once(':').map(|(h, _)| h).unwrap_or(authority).to_string(),
    }
}

/// The `http(s)://` origin serving [`protocol::WS_CHALLENGE_PATH`], derived
/// from the WS URL so there is nothing extra to configure.
fn challenge_url(ws_url: &str) -> String {
    let http = if let Some(rest) = ws_url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = ws_url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        ws_url.to_string()
    };
    // Drop the WS path (`/v1/ws`) and hang the challenge path off the origin.
    let origin = match http.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            format!("{scheme}://{authority}")
        }
        None => http,
    };
    format!("{origin}{}", protocol::WS_CHALLENGE_PATH)
}

/// Ask the gateway for a single-use handshake challenge.
async fn fetch_challenge(ws_url: &str) -> anyhow::Result<String> {
    let url = challenge_url(ws_url);
    let resp = crate::http::plain()
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("{url} returned {}", resp.status());
    }
    let body: serde_json::Value = resp.json().await?;
    let challenge = body
        .get("challenge")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{url} returned no challenge"))?;
    Ok(challenge.to_string())
}

/// Run one connected session until the socket closes, a control kick arrives, or
/// shutdown is requested.
async fn run_session(
    cfg: &WsConfig,
    deps: &PublisherDeps,
    shutdown_rx: &mut watch::Receiver<bool>,
    state_tx: &watch::Sender<ConnState>,
    nudge_rx: &watch::Receiver<u64>,
    resume_rx: &watch::Receiver<ResumeRequest>,
) -> SessionOutcome {
    // One-shot challenge from the gateway we are about to open a socket to.
    // Signing it (and the gateway's own name) is what makes a captured
    // handshake worthless: it cannot be replayed here, and it cannot be
    // forwarded to a different gateway.
    let audience = gateway_audience(&cfg.gateway_ws_url);
    let challenge = match fetch_challenge(&cfg.gateway_ws_url).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("could not obtain a handshake challenge: {e}");
            return SessionOutcome::Closed { was_online: false };
        }
    };

    let ts = now_ms();
    let nonce = uuid::Uuid::new_v4().to_string();
    let sig = deps.identity.sign_handshake(&cfg.device_id, ts, &nonce, &audience, &challenge);

    let mut req = match cfg.gateway_ws_url.clone().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("bad ws url: {e}");
            return SessionOutcome::Closed { was_online: false };
        }
    };
    {
        let h = req.headers_mut();
        let set = |h: &mut tokio_tungstenite::tungstenite::http::HeaderMap, k: &'static str, v: String| {
            if let Ok(val) = v.parse() {
                h.insert(k, val);
            }
        };
        set(h, "authorization", format!("Bearer {}", cfg.device_token));
        set(h, protocol::H_DEVICE, cfg.device_id.clone());
        set(h, protocol::H_TS, ts.to_string());
        set(h, protocol::H_NONCE, nonce.clone());
        set(h, protocol::H_SIG, sig);
        set(h, protocol::H_AUDIENCE, audience.clone());
        set(h, protocol::H_CHALLENGE, challenge.clone());
        // The upgrade request carries no `User-Agent`, so this header is the
        // only thing that tells the gateway which build is asking to publish —
        // and therefore the only way it can refuse an outdated seller before the
        // socket exists.
        set(h, protocol::H_CLIENT_VERSION, crate::http::VERSION.to_string());
    }

    // Raise the frame/message ceiling to what the gateway advertises. Left at
    // the tungstenite defaults (16 MiB per frame), a prompt with large inlined
    // attachments arrives as one oversized `http_request` frame, and the
    // resulting protocol error drops the session — taking every other in-flight
    // task on this socket with it.
    let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(protocol::MAX_FRAME_BYTES),
        max_frame_size: Some(protocol::MAX_FRAME_BYTES),
        ..Default::default()
    };
    let ws = match tokio_tungstenite::connect_async_with_config(req, Some(ws_config), false).await {
        Ok((ws, _)) => ws,
        Err(e) => {
            // 426 is the platform saying this build is too old to sell. It is
            // not a transport failure and reconnecting will never fix it, so it
            // is recorded for the UI instead of just being logged — the seller's
            // only symptom otherwise is a publisher that silently never comes
            // online.
            if let tokio_tungstenite::tungstenite::Error::Http(resp) = &e {
                if resp.status() == tokio_tungstenite::tungstenite::http::StatusCode::UPGRADE_REQUIRED {
                    crate::upgrade::record(&min_version_from_body(resp.body().as_deref()), "sell");
                    return SessionOutcome::Closed { was_online: false };
                }
            }
            tracing::warn!("ws connect failed: {e}");
            return SessionOutcome::Closed { was_online: false };
        }
    };
    // Reaching this point is the proof that the build is acceptable again.
    crate::upgrade::clear();
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Envelope>();

    // Hello + supply declaration up front.
    //
    // The hello carries the device id and nothing else. It used to also send
    // the app version and `std::env::consts::OS`; the gateway has no arm for
    // `hello` at all (it authenticates the device from the handshake headers
    // and drops the frame), so those two fields were telemetry that no one
    // read — and a client that ships them cannot honestly say it doesn't
    // profile the machine it runs on. Anything the gateway genuinely needs
    // about this device arrives signed, in the handshake.
    //
    // Every declaration is a full snapshot of what this device currently
    // offers, stamped with a per-session sequence so the gateway can drop one
    // that arrives out of order (a nudge and a periodic tick can overlap) and
    // withdraw whatever is missing from the newest one.
    let seq = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let _ = out_tx.send(Envelope::new(
        protocol::T_HELLO,
        serde_json::json!({"device_id": cfg.device_id}),
    ));
    let items = deps.supply.declare_items().await;
    let _ = out_tx.send(Envelope::new(
        protocol::T_SUPPLY_DECLARE,
        serde_json::json!({"items": items, "seq": next_seq(&seq)}),
    ));

    // Write loop.
    let writer = tokio::spawn(async move {
        while let Some(env) = out_rx.recv().await {
            if let Ok(txt) = serde_json::to_string(&env) {
                if ws_tx.send(Message::Text(txt)).await.is_err() {
                    break;
                }
            }
        }
        let _ = ws_tx.send(Message::Close(None)).await;
    });

    // Heartbeat loop (spec §9.2).
    let hb_tx = out_tx.clone();
    let heartbeat = tokio::spawn(async move {
        let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            tick.tick().await;
            if hb_tx.send(Envelope::new(protocol::T_HEARTBEAT, serde_json::json!({"ts": now_ms()}))).is_err() {
                break;
            }
        }
    });

    // Supply re-declaration: on demand (`nudge`) for real changes, periodically
    // for quota drift (spec §5.1).
    let supply = deps.supply.clone();
    let supply_tx = out_tx.clone();
    let supply_seq = seq.clone();
    let mut nudge = nudge_rx.clone();
    nudge.borrow_and_update(); // a nudge from before this session is not ours
    let mut resume = resume_rx.clone();
    resume.borrow_and_update();
    let supply_refresh = tokio::spawn(async move {
        let mut tick = tokio::time::interval(SUPPLY_REFRESH);
        tick.tick().await; // skip immediate tick (already declared above)
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                changed = nudge.changed() => {
                    if changed.is_err() {
                        break; // the handle is gone; the session is ending
                    }
                }
                changed = resume.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    // Lift the gateway's exclusion first, then re-declare: a
                    // declaration that arrives while the lane is still
                    // quarantined is recorded but not indexed, so doing it the
                    // other way round would leave the lane out until the next
                    // periodic tick.
                    let model = resume.borrow_and_update().1.clone();
                    let frame = Envelope::new(protocol::T_SUPPLY_RESUME, serde_json::json!({"model": model}));
                    if supply_tx.send(frame).is_err() {
                        break;
                    }
                }
            }
            let items = supply.declare_items().await;
            let payload = serde_json::json!({"items": items, "seq": next_seq(&supply_seq)});
            if supply_tx.send(Envelope::new(protocol::T_SUPPLY_UPDATE, payload)).is_err() {
                break;
            }
        }
    });

    // Serving a lease means calling the provider upstream: proxy-aware client.
    let http = crate::http::upstream();
    let mut was_online = false;
    // The key this build pins, resolved before the socket was opened. The
    // gateway's own `quota_pubkey` is compared against it below but never
    // replaces it: a key learned from the connection it is meant to vouch for
    // protects against nothing.
    let quota = deps.quota.clone();
    let outcome;

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    outcome = SessionOutcome::Shutdown;
                    break;
                }
            }
            msg = tokio::time::timeout(IDLE_TIMEOUT, ws_rx.next()) => {
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => {
                        // Nothing inbound for a full timeout: the socket is
                        // alive only as far as this end can tell. Drop it and
                        // reconnect rather than sit here believing we are
                        // still on the market.
                        tracing::warn!("no frames from the gateway for {IDLE_TIMEOUT:?}; reconnecting");
                        outcome = SessionOutcome::Closed { was_online };
                        break;
                    }
                };
                match msg {
                    Some(Ok(Message::Text(txt))) => {
                        if let Ok(env) = serde_json::from_str::<Envelope>(&txt) {
                            match env.msg_type.as_str() {
                                protocol::T_HELLO_ACK => {
                                    // A gateway offering a key other than the
                                    // pinned one is either misconfigured or not
                                    // the gateway at all. Either way this
                                    // session is not one to serve work on.
                                    let offered = env
                                        .payload
                                        .get("quota_pubkey")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default();
                                    if !offered.is_empty() && offered != deps.quota_pubkey {
                                        tracing::error!(
                                            "gateway offered a quota public key this build does not trust — \
                                             refusing to serve on this connection"
                                        );
                                        outcome = SessionOutcome::Kicked;
                                        break;
                                    }
                                    was_online = true;
                                    let _ = state_tx.send(ConnState::Online);
                                }
                                protocol::T_HTTP_REQUEST => {
                                    if let Ok(reqp) = serde_json::from_value::<HttpRequestPayload>(env.payload) {
                                        spawn_execute(&http, deps, reqp, &out_tx, quota.clone());
                                    }
                                }
                                protocol::T_CONTROL => {
                                    if let Ok(ctrl) = serde_json::from_value::<ControlPayload>(env.payload.clone()) {
                                        match handle_control(&ctrl, state_tx, deps.lanes.as_deref()) {
                                            ControlResult::Kick => { outcome = SessionOutcome::Kicked; break; }
                                            ControlResult::Reconnect => { outcome = SessionOutcome::Migrate; break; }
                                            ControlResult::Continue => {}
                                        }
                                    }
                                }
                                protocol::T_PING => {
                                    let _ = out_tx.send(Envelope::new(protocol::T_PONG, serde_json::json!({"ts": now_ms()})));
                                }
                                // price.update / settle.notify / heartbeat.ack are
                                // consumed here; the Tauri layer observes via state.
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        // tungstenite auto-pongs, but forward explicit app pings too.
                        let _ = out_tx.send(Envelope::new(protocol::T_PONG, serde_json::json!({"echo_len": p.len()})));
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        outcome = SessionOutcome::Closed { was_online };
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::warn!("ws read error: {e}");
                        outcome = SessionOutcome::Closed { was_online };
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    // Teardown.
    heartbeat.abort();
    supply_refresh.abort();
    drop(out_tx); // closes the write loop → sends Close frame.
    let _ = writer.await;
    outcome
}

fn spawn_execute(
    http: &reqwest::Client,
    deps: &PublisherDeps,
    req: HttpRequestPayload,
    out: &mpsc::UnboundedSender<Envelope>,
    quota: Arc<QuotaVerifier>,
) {
    let http = http.clone();
    let tokens = deps.tokens.clone();
    let records = deps.records.clone();
    let out = out.clone();
    tokio::spawn(async move {
        executor::execute(&http, tokens.as_ref(), req, &out, records.as_deref(), &quota).await;
    });
}

enum ControlResult {
    Kick,
    /// The gateway node holding this socket is being replaced. End the session
    /// and reconnect at once — a different node will pick it up.
    Reconnect,
    Continue,
}

fn handle_control(
    ctrl: &ControlPayload,
    state_tx: &watch::Sender<ConnState>,
    lanes: Option<&dyn LaneControl>,
) -> ControlResult {
    match ctrl.action.as_str() {
        "lane.pause" => {
            tracing::warn!(model = %ctrl.model, "gateway paused a lane: {}", ctrl.reason);
            if let Some(l) = lanes {
                l.on_lane_pause(&ctrl.model, &ctrl.reason, ctrl.resume_requires_user);
            }
            ControlResult::Continue
        }
        // Reputation standing, reported on every supply declaration. Recorded
        // rather than acted on: nothing the client can do about it, but a seller
        // getting no traffic deserves to see the reason without being told to
        // read a server log.
        "seller.status" => {
            crate::seller_status::record(ctrl.score, ctrl.min_score);
            ControlResult::Continue
        }
        "kick" => {
            tracing::warn!("server kicked device: {}", ctrl.reason);
            ControlResult::Kick
        }
        // Deliberately *not* a kick. A kick is terminal — it stops the
        // reconnect loop and leaves the device offline until a human acts.
        // A node being replaced during a deploy wants the opposite: go away
        // and come straight back somewhere else.
        "reconnect" => {
            tracing::info!("gateway asked us to reconnect: {}", ctrl.reason);
            ControlResult::Reconnect
        }
        "throttle" => {
            tracing::info!("server throttle {}ms: {}", ctrl.throttle_ms, ctrl.reason);
            let _ = state_tx.send(ConnState::Throttled);
            ControlResult::Continue
        }
        "upgrade" => {
            tracing::info!("server requests client upgrade: {}", ctrl.reason);
            ControlResult::Continue
        }
        other => {
            tracing::debug!("unknown control action: {other}");
            ControlResult::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::SinkExt;
    use tokio::net::TcpListener;

    struct FixedConfig {
        url: String,
    }
    #[async_trait]
    impl ConfigSource for FixedConfig {
        async fn ws_config(&self) -> anyhow::Result<WsConfig> {
            Ok(WsConfig {
                gateway_ws_url: self.url.clone(),
                device_id: "dev-test".into(),
                device_token: "tok".into(),
            })
        }
    }

    struct FixedSupply;
    #[async_trait]
    impl SupplySource for FixedSupply {
        async fn declare_items(&self) -> serde_json::Value {
            serde_json::json!([{ "model": "claude-x", "provider": "claude", "window_remaining": 1000, "price_min": 10 }])
        }
    }

    struct NoTokens;
    impl TokenProvider for NoTokens {
        fn token_for(&self, _p: &str) -> Option<String> {
            None
        }
    }

    #[test]
    fn backoff_grows_and_caps() {
        let cap = Duration::from_secs(60);
        let mut d = Duration::from_secs(1);
        let mut seen_cap = false;
        for _ in 0..12 {
            d = next_backoff(d, cap);
            // Never exceeds cap + jitter headroom.
            assert!(d <= cap + Duration::from_secs(1));
            if d >= Duration::from_secs(48) {
                seen_cap = true;
            }
        }
        assert!(seen_cap, "backoff should climb toward the cap");
    }

    /// Serve one plaintext `GET /v1/ws/challenge` on `listener`, then hand the
    /// next connection back for the WS upgrade.
    ///
    /// The client asks the gateway for a single-use challenge before it opens
    /// the socket, so every test gateway has to answer that first — which is
    /// the point: there is no code path that connects without one.
    async fn serve_challenge(listener: &TcpListener) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        assert!(
            String::from_utf8_lossy(&buf[..n]).contains(protocol::WS_CHALLENGE_PATH),
            "the client must fetch a challenge before connecting"
        );
        let body = r#"{"challenge":"test-challenge","expires_in":30}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    }

    fn deps() -> PublisherDeps {
        let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        let quota_pubkey = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes())
        };
        PublisherDeps {
            quota: Arc::new(QuotaVerifier::from_pubkey_b64(&quota_pubkey).unwrap()),
            quota_pubkey,
            identity: Arc::new(DeviceIdentity::generate()),
            tokens: Arc::new(NoTokens),
            supply: Arc::new(FixedSupply),
            records: None,
            lanes: None,
        }
    }

    /// A minimal WS server that accepts a connection, checks the auth headers are
    /// present, sends hello.ack, and asserts it receives hello + supply.declare.
    #[tokio::test]
    async fn handshake_declares_supply_and_goes_online() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (got_tx, got_rx) = tokio::sync::oneshot::channel::<Vec<String>>();

        tokio::spawn(async move {
            serve_challenge(&listener).await;
            let (stream, _) = listener.accept().await.unwrap();
            // Verify headers during the handshake callback.
            let mut saw_sig = false;
            let mut saw_device = false;
            let callback = |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                            resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                let h = req.headers();
                saw_sig = h.contains_key("x-asale-sig");
                saw_device = h.get("x-asale-device").and_then(|v| v.to_str().ok()) == Some("dev-test");
                Ok(resp)
            };
            let mut ws = tokio_tungstenite::accept_hdr_async(stream, callback).await.unwrap();
            assert!(saw_sig && saw_device, "auth headers must be present");

            // Send hello.ack so the client transitions to Online.
            let ack = Envelope::new(protocol::T_HELLO_ACK, serde_json::json!({"heartbeat_sec": 30}));
            ws.send(Message::Text(serde_json::to_string(&ack).unwrap())).await.unwrap();

            // Collect the first two inbound frame types (hello + supply.declare).
            let mut types = Vec::new();
            while types.len() < 2 {
                if let Some(Ok(Message::Text(t))) = ws.next().await {
                    if let Ok(env) = serde_json::from_str::<Envelope>(&t) {
                        types.push(env.msg_type);
                    }
                } else {
                    break;
                }
            }
            let _ = got_tx.send(types);
        });

        let cfg = Arc::new(FixedConfig { url: format!("ws://127.0.0.1:{port}/v1/ws") });
        let handle = spawn_publisher(cfg, deps());

        // Wait for Online.
        let mut rx = handle.state_receiver();
        let online = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if *rx.borrow() == ConnState::Online {
                    return true;
                }
                if rx.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(online, "client should reach Online after hello.ack");

        let types = tokio::time::timeout(Duration::from_secs(3), got_rx).await.unwrap().unwrap();
        assert!(types.contains(&protocol::T_HELLO.to_string()));
        assert!(types.contains(&protocol::T_SUPPLY_DECLARE.to_string()));

        handle.stop();
    }

    /// A change to what this device sells has to reach the gateway now, not at
    /// the next periodic re-declaration — until it lands, the market keeps
    /// dispatching against capacity that has been withdrawn.
    #[tokio::test]
    async fn a_nudge_redeclares_supply_without_waiting_for_the_tick() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (got_tx, got_rx) = tokio::sync::oneshot::channel::<(String, u64)>();

        tokio::spawn(async move {
            serve_challenge(&listener).await;
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let ack = Envelope::new(protocol::T_HELLO_ACK, serde_json::json!({}));
            ws.send(Message::Text(serde_json::to_string(&ack).unwrap())).await.unwrap();

            let mut first_seq = 0;
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let Ok(env) = serde_json::from_str::<Envelope>(&t) else { continue };
                let seq = env.payload.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
                match env.msg_type.as_str() {
                    protocol::T_SUPPLY_DECLARE => first_seq = seq,
                    protocol::T_SUPPLY_UPDATE => {
                        assert!(seq > first_seq, "each snapshot carries a higher seq");
                        let _ = got_tx.send((env.msg_type, seq));
                        return;
                    }
                    _ => {}
                }
            }
        });

        let cfg = Arc::new(FixedConfig { url: format!("ws://127.0.0.1:{port}/v1/ws") });
        let handle = spawn_publisher(cfg, deps());
        let mut rx = handle.state_receiver();
        while *rx.borrow() != ConnState::Online {
            rx.changed().await.unwrap();
        }

        handle.nudge();
        let (kind, seq) = tokio::time::timeout(Duration::from_secs(3), got_rx)
            .await
            .expect("a nudge must re-declare long before the periodic tick")
            .unwrap();
        assert_eq!(kind, protocol::T_SUPPLY_UPDATE);
        assert!(seq >= 2);
        assert!(SUPPLY_REFRESH > Duration::from_secs(3), "otherwise the tick, not the nudge, could have sent it");
        handle.stop();
    }

    #[tokio::test]
    async fn control_kick_stops_reconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            serve_challenge(&listener).await;
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let ack = Envelope::new(protocol::T_HELLO_ACK, serde_json::json!({}));
            ws.send(Message::Text(serde_json::to_string(&ack).unwrap())).await.unwrap();
            let kick = Envelope::new(protocol::T_CONTROL, serde_json::json!({"action": "kick", "reason": "test"}));
            ws.send(Message::Text(serde_json::to_string(&kick).unwrap())).await.unwrap();
            // Keep the socket open a moment so the client processes the kick.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let cfg = Arc::new(FixedConfig { url: format!("ws://127.0.0.1:{port}/v1/ws") });
        let handle = spawn_publisher(cfg, deps());
        let mut rx = handle.state_receiver();
        let kicked = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if *rx.borrow() == ConnState::Kicked {
                    return true;
                }
                if rx.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(kicked, "kick control should move to Kicked and stop the loop");
    }

    #[tokio::test]
    async fn control_reconnect_moves_the_session_instead_of_ending_it() {
        // The distinction that makes zero-downtime deploys possible. A gateway
        // node being replaced sends `reconnect`; if the client treated that as a
        // kick, every release would take the whole fleet of publishers off the
        // market until each user noticed and intervened.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (connects_tx, mut connects_rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn(async move {
            for attempt in 0..2u32 {
                serve_challenge(&listener).await;
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let ack = Envelope::new(protocol::T_HELLO_ACK, serde_json::json!({}));
                ws.send(Message::Text(serde_json::to_string(&ack).unwrap())).await.unwrap();
                let _ = connects_tx.send(attempt);
                if attempt == 0 {
                    let mv = Envelope::new(
                        protocol::T_CONTROL,
                        serde_json::json!({"action": "reconnect", "reason": "node draining"}),
                    );
                    ws.send(Message::Text(serde_json::to_string(&mv).unwrap())).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(200)).await;
                } else {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        });

        let cfg = Arc::new(FixedConfig { url: format!("ws://127.0.0.1:{port}/v1/ws") });
        let handle = spawn_publisher(cfg, deps());
        let mut rx = handle.state_receiver();

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), connects_rx.recv()).await.unwrap(),
            Some(0)
        );
        // The point of the test: it comes back, and quickly.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), connects_rx.recv()).await
                .expect("a reconnect notice must not end the publisher loop"),
            Some(1)
        );
        let back = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match *rx.borrow() {
                    ConnState::Online => return true,
                    ConnState::Kicked | ConnState::Offline => return false,
                    _ => {}
                }
                if rx.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(back, "the device must be back online, not kicked or offline");
        handle.stop();
    }
}
