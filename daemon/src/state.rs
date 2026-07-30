//! Shared daemon state. Owned by the HTTP layer and every background task
//! (consumer proxy, publisher session, token-refresh loop, usage aggregation).

use crate::keychain;
use asale_client_core::config::ClientConfig;
use asale_client_core::store::LocalStore;
use asale_client_core::{AccountPool, DeviceIdentity, PublisherHandle, Strategy};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

/// Result slot of an in-flight browser OAuth flow (two-step RPC: the frontend
/// opens `auth_url` and polls `oauth_result` with the flow id).
#[derive(Clone)]
pub enum FlowStatus {
    Pending,
    Done(serde_json::Value),
    /// Keeps the failure's translation key, not just its English text — the
    /// frontend renders an async flow's error the same way it renders a
    /// synchronous one (`commands::CmdError`).
    Failed(crate::commands::CmdError),
}

pub struct AppState {
    pub cfg: ClientConfig,
    pub store: Arc<LocalStore>,
    /// The asale consumer API key (mirrored into the proxy), kept in the
    /// encrypted on-disk secret store.
    pub asale_key: Arc<RwLock<Option<String>>>,
    /// The running publisher session (WS relay client), if publishing.
    pub publisher: Arc<RwLock<Option<PublisherHandle>>>,
    /// Background token-refresh loop handle (runs while publishing).
    pub refresh_task: Arc<RwLock<Option<JoinHandle<()>>>>,
    /// Device Ed25519 identity (seed persisted in the encrypted secret store).
    pub identity: Arc<DeviceIdentity>,
    /// device_id for this install.
    pub device_id: String,
    /// Multi-account pool (spec §4) shared by the executor and consumer proxy.
    /// std Mutex: `TokenProvider` is a sync trait and holders never await.
    pub pool: Arc<std::sync::Mutex<AccountPool>>,
    /// Cache of live provider rate-limit windows (Claude `oauth/usage`), keyed
    /// by provider → (fetched_at unix secs, windows JSON array, or the reason the
    /// fetch failed). Avoids hammering the upstream usage endpoint on every
    /// Limits-page poll; failures are cached too, so an unreachable upstream
    /// costs one timeout per half-minute rather than one per poll.
    pub limits_cache: Arc<RwLock<std::collections::HashMap<String, (i64, Result<serde_json::Value, String>)>>>,
    /// In-flight browser OAuth flows: flow_id → status. Entries are removed
    /// when the frontend collects a terminal result.
    pub oauth_flows: Arc<RwLock<HashMap<String, FlowStatus>>>,
    /// Lane state changes posted from the executor's hot path (which holds the
    /// pool's sync lock and cannot await), drained by `spawn_lane_loop`.
    pub lane_tx: crate::publisher::LaneSender,
    /// Parked until the daemon starts its background tasks; see `take_lane_rx`.
    lane_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<crate::publisher::LaneEvent>>>,
}

impl AppState {
    /// Claim the lane-event receiver. Returns None after the first call — there
    /// is exactly one drain loop.
    pub fn take_lane_rx(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<crate::publisher::LaneEvent>> {
        self.lane_rx.lock().ok()?.take()
    }
}

impl AppState {
    pub async fn new() -> anyhow::Result<AppState> {
        let cfg = ClientConfig::default();
        // Fail fast rather than relaying the user's prompts and device token
        // over an unencrypted link to a remote host.
        cfg.validate()?;
        let dir = data_dir();
        std::fs::create_dir_all(&dir).ok();
        let db_path = format!("{}/asale.db", dir);
        let store = Arc::new(LocalStore::open(&db_path).await?);

        // Stable device id (persisted in settings).
        let device_id = match store.get_setting("device_id").await? {
            Some(id) => id,
            None => {
                let id = format!("dev-{}", uuid::Uuid::new_v4().simple());
                store.set_setting("device_id", &id).await?;
                id
            }
        };

        // Device signing identity: seed lives in the encrypted secret store (§10.6).
        let identity = Arc::new(load_or_create_identity()?);

        // Seed the account pool from the store; kept fresh by the refresh loop
        // and rebuilt on every account change.
        let pool = Arc::new(std::sync::Mutex::new(AccountPool::new(Strategy::RoundRobin)));
        crate::publisher::rebuild_pool(&store, &pool).await;

        // The receiver is parked here until the daemon's background tasks
        // start and `take_lane_rx` hands it to `spawn_lane_loop`.
        let (lane_tx, lane_rx) = tokio::sync::mpsc::unbounded_channel();

        Ok(AppState {
            cfg,
            store,
            asale_key: Arc::new(RwLock::new(None)),
            publisher: Arc::new(RwLock::new(None)),
            refresh_task: Arc::new(RwLock::new(None)),
            identity,
            device_id,
            pool,
            limits_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
            oauth_flows: Arc::new(RwLock::new(HashMap::new())),
            lane_tx,
            lane_rx: std::sync::Mutex::new(Some(lane_rx)),
        })
    }
}

/// Load the device Ed25519 seed from the encrypted secret store, generating +
/// persisting one on first run.
fn load_or_create_identity() -> anyhow::Result<DeviceIdentity> {
    if let Some(seed) = keychain::get(keychain::DEVICE_SEED)? {
        if let Ok(id) = DeviceIdentity::from_seed_b64(&seed) {
            return Ok(id);
        }
    }
    let id = DeviceIdentity::generate();
    keychain::set(keychain::DEVICE_SEED, &id.seed_b64())?;
    Ok(id)
}

/// The app data dir — `$ASALE_DATA_DIR` if set, else `~/.asale` (matches the
/// keychain module and the SQLite store).
///
/// `$HOME` is not the only spelling of "home": Windows sets `USERPROFILE` and
/// leaves `HOME` unset in PowerShell, cmd, and anything launched from Explorer
/// or a shortcut. Falling straight through to a *relative* `.asale` meant the
/// same installation silently kept two states — the real one under the user's
/// profile, and an empty one beside whatever directory the app happened to be
/// launched from, with its own device id, its own `daemon.token` and no
/// accounts. That reads exactly like data loss. The relative path stays as the
/// last resort, for a platform that offers neither variable.
pub fn data_dir() -> String {
    if let Ok(d) = std::env::var("ASALE_DATA_DIR") {
        return d;
    }
    for var in ["HOME", "USERPROFILE"] {
        match std::env::var(var) {
            Ok(home) if !home.trim().is_empty() => return format!("{home}/.asale"),
            _ => continue,
        }
    }
    // Windows also splits the profile path across two variables; join them
    // rather than writing state next to the executable.
    if let (Ok(drive), Ok(path)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        if !drive.trim().is_empty() && !path.trim().is_empty() {
            return format!("{drive}{path}/.asale");
        }
    }
    ".asale".to_string()
}
