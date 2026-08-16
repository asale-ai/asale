//! Local SQLite store (spec §8). Holds tool refs (secret-store pointers only, no
//! plaintext), publish config, records. Credentials live in the encrypted
//! on-disk secret store (`keychain_ref` names the entry there).

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use std::str::FromStr;

pub struct LocalStore {
    pub pool: SqlitePool,
}

/// One locally installed AI CLI's buy switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuyToolRow {
    pub tool: String,
    pub enabled: bool,
    /// Model ids this tool may buy. Empty = no restriction.
    pub models: Vec<String>,
    /// Verbatim pre-switch config files, `None` when nothing is stashed.
    pub backup_json: Option<String>,
    /// When the switch was turned on; 0 while off.
    pub since_ts: i64,
}

impl BuyToolRow {
    /// The state of a tool that has never been switched on.
    pub fn off(tool: &str) -> BuyToolRow {
        BuyToolRow {
            tool: tool.to_string(),
            enabled: false,
            models: Vec::new(),
            backup_json: None,
            since_ts: 0,
        }
    }
}

/// An imported tool account row (keychain reference + metadata only).
#[derive(Debug, Clone)]
pub struct ToolRow {
    pub provider: String,
    pub account_id: String,
    pub keychain_ref: String,
    pub plan: Option<String>,
    /// Where the credential came from: `oauth` (asale ran its own browser login,
    /// so asale exclusively owns this token) or `import` (copied from a locally
    /// installed CLI, so the upstream refresh token is shared with that CLI).
    pub origin: Option<String>,
    /// Human-readable origin detail of the credential actually in use
    /// (keychain service or file path).
    pub source: Option<String>,
    /// Every local store found holding this same subscription account. One
    /// account is one row however many stores hold its token; this is what the
    /// UI lists so a merged row can be explained.
    pub sources: Vec<String>,
    /// Per-account sell switch (spec: each subscription account is sold
    /// independently, not per provider family).
    pub sell_enabled: bool,
    /// Per-account daily sell cap in tokens; 0 = unlimited.
    pub sell_daily_limit: i64,
    /// The price band this account will sell inside, as whole percent **of the
    /// vendor's list price** — 100 is list price, 60 is "six-tenths off list".
    /// A model the market currently prices outside `[min, max]` is withheld
    /// until it comes back.
    ///
    /// This is the ratio, not the discount off it, because that is the number
    /// the seller is actually deciding about ("I will not sell below 60% of
    /// list") and because it shares its range with the server's own
    /// `mkt_ratio`, which is clamped to `[0.05, 1.00]`.
    pub sell_min_ratio: i64,
    pub sell_max_ratio: i64,
    /// How many requests this account will serve at once.
    ///
    /// Declared to the market as the lane's `concurrency_total`, so it is the
    /// seller's own ceiling on how much work the gateway may have in flight
    /// against this subscription — matching stops offering the lane once that
    /// many tasks are outstanding, rather than the client having to refuse them
    /// after the fact. Per account, because the vendor's own rate limit is.
    pub sell_concurrency: i64,
}

/// The band's legal range, and the widest one there is: the server clamps
/// `mkt_ratio` to `[0.05, 1.00]`, so `5..=100` covers every price a model can
/// ever have and therefore can never withhold anything. It is the platform's
/// floor, not anybody's asking price.
pub const RATIO_BAND_FULL: (i64, i64) = (5, 100);

/// The floor an account sells at when nobody has said otherwise.
///
/// Deliberately above `RATIO_BAND_FULL.0`: the platform allows a seller to go
/// down to 5% of list, but a seller who has not thought about it should not be
/// offering their subscription at the cheapest price the market can reach.
/// There is no "any price" setting any more — every account trades against a
/// floor, and this is the one it starts on.
pub const DEFAULT_SELL_MIN_RATIO: i64 = 10;

/// What one subscription account serves at once when nobody has said
/// otherwise. Five is the number a single interactive CLI session comfortably
/// keeps busy without the vendor starting to 429 — high enough that a seller is
/// not leaving capacity idle, low enough that the default never gets an account
/// rate-limited on its owner's behalf.
pub const DEFAULT_SELL_CONCURRENCY: i64 = 5;

/// The range an operator may set concurrency to. The floor is 1 — an account
/// that serves nothing is expressed by switching selling off, not by a zero
/// here, and a zero would otherwise declare a lane the market can never pick.
/// The ceiling is a sanity bound on a hand-typed number, not a vendor limit.
pub const SELL_CONCURRENCY_RANGE: (i64, i64) = (1, 64);

/// Clamp a concurrency setting into its legal range.
///
/// A stored 0 — which is what a row written before this column existed reads as
/// under SQLite's `DEFAULT` for an added column, and what a half-typed input
/// produces — becomes the default rather than the floor: nobody chose 0, so it
/// means "unset", and answering it with 1 would quietly take four fifths of an
/// upgraded seller's capacity off the market.
pub fn normalise_concurrency(n: i64) -> i64 {
    if n <= 0 {
        return DEFAULT_SELL_CONCURRENCY;
    }
    n.clamp(SELL_CONCURRENCY_RANGE.0, SELL_CONCURRENCY_RANGE.1)
}

/// Clamp a price band into the legal range and put its ends the right way
/// round.
///
/// A non-positive end reads as "unset" rather than as the bottom of the range —
/// that is what a row written before this column existed carries, and what a
/// half-typed form produces. Unset means the default floor and list price
/// respectively; answering an unset floor with the platform's own 5% would
/// quietly halve what an upgraded seller asks.
pub fn normalise_band(min_ratio: i64, max_ratio: i64) -> (i64, i64) {
    let (floor, ceiling) = RATIO_BAND_FULL;
    let lo = if min_ratio <= 0 { DEFAULT_SELL_MIN_RATIO } else { min_ratio }.clamp(floor, ceiling);
    let hi = if max_ratio <= 0 { ceiling } else { max_ratio }.clamp(floor, ceiling);
    if lo > hi {
        (hi, lo)
    } else {
        (lo, hi)
    }
}

/// Base schema for a fresh database. Tables only — no index may reference a
/// column that `MIGRATIONS` adds, because this runs *before* the migrations and
/// an older database would still be missing that column ("no such column:
/// provider"), which aborts the whole schema step. Such indexes go in
/// `MIGRATIONS`, after the `ALTER TABLE` that creates their columns.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tools (
  id INTEGER PRIMARY KEY,
  provider TEXT NOT NULL,
  account_id TEXT NOT NULL,
  keychain_ref TEXT NOT NULL,
  plan TEXT, source TEXT,
  sources TEXT,
  discovered_at INTEGER,
  origin TEXT,
  sell_enabled INTEGER NOT NULL DEFAULT 0,
  sell_daily_limit INTEGER NOT NULL DEFAULT 0,
  sell_min_ratio INTEGER NOT NULL DEFAULT 10,
  sell_max_ratio INTEGER NOT NULL DEFAULT 100,
  sell_concurrency INTEGER NOT NULL DEFAULT 5,
  UNIQUE(provider, account_id)
);
CREATE TABLE IF NOT EXISTS publish_config (
  provider TEXT, model TEXT,
  enabled INTEGER, reserve_min INTEGER,
  rate_hourly INTEGER, rate_daily INTEGER,
  price_min INTEGER,
  PRIMARY KEY(provider, model)
);
CREATE TABLE IF NOT EXISTS provider_records (
  task_id TEXT PRIMARY KEY, ts INTEGER, model TEXT,
  in_tokens INTEGER, out_tokens INTEGER, cache_read INTEGER, cache_write INTEGER,
  amount_usdt INTEGER, counterparty_anon TEXT, status TEXT,
  provider TEXT NOT NULL DEFAULT '',
  account_id TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS wallet_txns (
  id INTEGER PRIMARY KEY, ts INTEGER, type TEXT,
  amount_usdt INTEGER, tx_hash TEXT, status TEXT
);
CREATE TABLE IF NOT EXISTS settings (k TEXT PRIMARY KEY, v TEXT);
CREATE TABLE IF NOT EXISTS usage_daily (
  source TEXT NOT NULL,          -- 'used' (this machine's own calls) | 'sold'
  day TEXT NOT NULL,             -- 'YYYY-MM-DD' (UTC)
  model TEXT NOT NULL,
  in_tokens INTEGER NOT NULL DEFAULT 0,
  out_tokens INTEGER NOT NULL DEFAULT 0,
  cache INTEGER NOT NULL DEFAULT 0,
  cnt INTEGER NOT NULL DEFAULT 0,
  amount INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(source, day, model)
);
CREATE TABLE IF NOT EXISTS usage_scan (
  path TEXT PRIMARY KEY,         -- a local CLI session log file
  byte_offset INTEGER NOT NULL DEFAULT 0,  -- last-parsed offset (append-only)
  mtime INTEGER NOT NULL DEFAULT 0
);
-- Lane pauses that outlive the process. Only the ones a person has to clear are
-- stored: a cooldown is measured in seconds and a restart is a fine excuse to
-- retry, but a lane the operator was asked to fix must still be paused when the
-- daemon comes back, or a restart would silently put broken capacity back on
-- the market.
CREATE TABLE IF NOT EXISTS lane_state (
  provider   TEXT NOT NULL,
  account_id TEXT NOT NULL,
  model      TEXT NOT NULL,
  reason     TEXT NOT NULL,      -- auth | breaker | manual
  last_error TEXT NOT NULL DEFAULT '',
  paused_at  INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(provider, account_id, model)
);
-- The buy switch of each locally installed AI CLI. These four columns used to
-- be four `settings` rows per tool under composed keys ('buy_enabled:claude',
-- 'buy_models:claude', …), which meant no types, no constraints, four queries
-- to read one tool's state, and no way to change two of them atomically.
CREATE TABLE IF NOT EXISTS buy_tools (
  tool        TEXT PRIMARY KEY,           -- claude | codex | gemini
  enabled     INTEGER NOT NULL DEFAULT 0,
  models_json TEXT NOT NULL DEFAULT '[]', -- selected model ids; [] = any
  backup_json TEXT NOT NULL DEFAULT '',   -- verbatim pre-switch config files
  since_ts    INTEGER NOT NULL DEFAULT 0  -- when the switch was turned on
);
"#;

/// Columns added after the first release. `CREATE TABLE IF NOT EXISTS` leaves an
/// existing table untouched, so bring older databases forward with `ALTER TABLE`;
/// a "duplicate column name" error just means this one already landed.
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE tools ADD COLUMN origin TEXT",
    "ALTER TABLE tools ADD COLUMN sell_enabled INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE tools ADD COLUMN sell_daily_limit INTEGER NOT NULL DEFAULT 0",
    // The price band the account sells inside, as percent of list price.
    // The floor defaults to `DEFAULT_SELL_MIN_RATIO`, not to the bottom of the
    // legal range: an account nobody has set a price on sells at 10% of list or
    // better, rather than at whatever the market's own floor happens to be.
    "ALTER TABLE tools ADD COLUMN sell_min_ratio INTEGER NOT NULL DEFAULT 10",
    "ALTER TABLE tools ADD COLUMN sell_max_ratio INTEGER NOT NULL DEFAULT 100",
    // How many requests the account serves at once, declared to the market as
    // the lane's concurrency ceiling. Defaulted to the same 5 a fresh row gets;
    // `normalise_concurrency` maps a 0 from an older row onto it too.
    "ALTER TABLE tools ADD COLUMN sell_concurrency INTEGER NOT NULL DEFAULT 5",
    // JSON array of every local store holding this account's token.
    "ALTER TABLE tools ADD COLUMN sources TEXT",
    "ALTER TABLE provider_records ADD COLUMN provider TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE provider_records ADD COLUMN account_id TEXT NOT NULL DEFAULT ''",
    "CREATE INDEX IF NOT EXISTS idx_provider_records_acct ON provider_records(provider, account_id, ts)",
    // Fold the old composed-key `settings` rows into `buy_tools`, then drop
    // them. Self-clearing: once the DELETE has run there is nothing left to
    // select, so re-running this on every open is a no-op.
    "INSERT OR IGNORE INTO buy_tools(tool) \
     SELECT DISTINCT substr(k, instr(k, ':') + 1) FROM settings \
     WHERE k LIKE 'buy_enabled:%' OR k LIKE 'buy_models:%' \
        OR k LIKE 'buy_backup:%' OR k LIKE 'buy_since:%'",
    "UPDATE buy_tools SET enabled = COALESCE(\
       (SELECT v = '1' FROM settings WHERE k = 'buy_enabled:' || tool), enabled)",
    "UPDATE buy_tools SET models_json = COALESCE(\
       (SELECT v FROM settings WHERE k = 'buy_models:' || tool AND v <> ''), models_json)",
    "UPDATE buy_tools SET backup_json = COALESCE(\
       (SELECT v FROM settings WHERE k = 'buy_backup:' || tool), backup_json)",
    "UPDATE buy_tools SET since_ts = COALESCE(\
       (SELECT CAST(v AS INTEGER) FROM settings WHERE k = 'buy_since:' || tool), since_ts)",
    "DELETE FROM settings WHERE k LIKE 'buy_enabled:%' OR k LIKE 'buy_models:%' \
        OR k LIKE 'buy_backup:%' OR k LIKE 'buy_since:%'",
    // The selling price is a floor now, not a window: the UI only asks for
    // "sell at or above X% of list", because no price is too good to accept. A
    // ceiling left over from when it did ask would withhold models on a rule
    // the operator can no longer see or lift, so raise every one to list price.
    // Idempotent — after this there is nothing left to update.
    "UPDATE tools SET sell_max_ratio = 100 WHERE sell_max_ratio < 100",
    // `consume_records` and everything folded from it are gone. It mirrored two
    // unrelated things and got both wrong: market purchases (recorded with an
    // amount of zero, because the price is struck at settlement after the
    // consumer's stream has closed, and with token counts read off the relayed
    // response rather than off the bill) and direct-route calls, which are this
    // machine's own subscription answering its own tool and now fold straight
    // into `usage_daily` as `used`. The buy side reads the server's ledger.
    //
    // The snapshot rows have to go with the table: they are the wrong numbers,
    // already summed, and nothing recomputes them. The cursor too, so a
    // reinstall over an old data directory does not start mid-file.
    "DELETE FROM usage_daily WHERE source = 'bought'",
    "DELETE FROM settings WHERE k = 'usage_agg_rowid:consume_records'",
    "DROP TABLE IF EXISTS consume_records",
];

async fn migrate(pool: &SqlitePool) {
    for stmt in MIGRATIONS {
        // Already-applied migrations fail with "duplicate column name"; anything
        // else is worth a log but must not stop the daemon from starting.
        if let Err(e) = sqlx::query(stmt).execute(pool).await {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                tracing::debug!("store migration skipped ({stmt}): {msg}");
            }
        }
    }
}

impl LocalStore {
    /// Open (creating if needed) the SQLite database at `path`.
    pub async fn open(path: &str) -> anyhow::Result<LocalStore> {
        // WAL + NORMAL + busy timeout: safe concurrent readers/writer across
        // the proxy/publisher/UI tasks, resilient to crash mid-write (§12).
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new().max_connections(4).connect_with(opts).await?;
        sqlx::query(SCHEMA).execute(&pool).await?;
        migrate(&pool).await;
        Ok(LocalStore { pool })
    }

    /// In-memory store for tests.
    pub async fn open_memory() -> anyhow::Result<LocalStore> {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await?;
        sqlx::query(SCHEMA).execute(&pool).await?;
        migrate(&pool).await;
        Ok(LocalStore { pool })
    }

    // ── Buy switches (one row per locally installed AI CLI) ─────────────────

    /// A tool's buy state. Returns the all-off default for a tool that has
    /// never been switched on, so callers never handle "no row yet".
    pub async fn buy_tool(&self, tool: &str) -> anyhow::Result<BuyToolRow> {
        let row: Option<(i64, String, String, i64)> = sqlx::query_as(
            "SELECT enabled, models_json, backup_json, since_ts FROM buy_tools WHERE tool = ?",
        )
        .bind(tool)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((enabled, models_json, backup_json, since_ts)) => BuyToolRow {
                tool: tool.to_string(),
                enabled: enabled != 0,
                models: serde_json::from_str(&models_json).unwrap_or_default(),
                backup_json: Some(backup_json).filter(|s| !s.is_empty()),
                since_ts,
            },
            None => BuyToolRow::off(tool),
        })
    }

    /// Update only the fields given. Every `None` leaves its column alone, so a
    /// model-selection edit cannot clobber the switch or the stored backup.
    pub async fn set_buy_tool(
        &self,
        tool: &str,
        enabled: Option<bool>,
        models: Option<&[String]>,
        backup_json: Option<&str>,
        since_ts: Option<i64>,
    ) -> anyhow::Result<()> {
        let models_json = models.map(serde_json::to_string).transpose()?;
        sqlx::query(
            "INSERT INTO buy_tools(tool, enabled, models_json, backup_json, since_ts)
             VALUES(?1, COALESCE(?2, 0), COALESCE(?3, '[]'), COALESCE(?4, ''), COALESCE(?5, 0))
             ON CONFLICT(tool) DO UPDATE SET
               enabled     = COALESCE(?2, enabled),
               models_json = COALESCE(?3, models_json),
               backup_json = COALESCE(?4, backup_json),
               since_ts    = COALESCE(?5, since_ts)",
        )
        .bind(tool)
        .bind(enabled.map(i64::from))
        .bind(models_json)
        .bind(backup_json)
        .bind(since_ts)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist an operator-clearable lane pause (auth / breaker / manual).
    pub async fn set_lane_pause(
        &self,
        provider: &str,
        account_id: &str,
        model: &str,
        reason: &str,
        last_error: &str,
        at: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO lane_state(provider, account_id, model, reason, last_error, paused_at)
             VALUES(?,?,?,?,?,?)
             ON CONFLICT(provider, account_id, model) DO UPDATE
               SET reason=excluded.reason, last_error=excluded.last_error, paused_at=excluded.paused_at",
        )
        .bind(provider)
        .bind(account_id)
        .bind(model)
        .bind(reason)
        .bind(last_error)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Forget a lane pause. An empty `model` clears every lane of the account;
    /// an empty `provider` clears everything.
    pub async fn clear_lane_pause(&self, provider: &str, account_id: &str, model: &str) -> anyhow::Result<()> {
        sqlx::query(
            "DELETE FROM lane_state
             WHERE (?1 = '' OR provider = ?1) AND (?2 = '' OR account_id = ?2) AND (?3 = '' OR model = ?3)",
        )
        .bind(provider)
        .bind(account_id)
        .bind(model)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every persisted lane pause: (provider, account_id, model, reason, last_error).
    pub async fn list_lane_pauses(&self) -> anyhow::Result<Vec<(String, String, String, String, String)>> {
        let rows = sqlx::query_as("SELECT provider, account_id, model, reason, last_error FROM lane_state")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    pub async fn set_setting(&self, k: &str, v: &str) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO settings(k,v) VALUES(?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v")
            .bind(k)
            .bind(v)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_setting(&self, k: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT v FROM settings WHERE k=?")
            .bind(k)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }

    /// List imported tool accounts (keychain refs only; no plaintext).
    pub async fn list_tools(&self) -> anyhow::Result<Vec<ToolRow>> {
        #[allow(clippy::type_complexity)]
        let rows: Vec<(String, String, String, Option<String>, Option<String>, Option<String>, Option<String>, i64, i64, i64, i64, i64)> =
            sqlx::query_as(
                "SELECT provider, account_id, keychain_ref, plan, origin, source, sources, sell_enabled, sell_daily_limit,
                        sell_min_ratio, sell_max_ratio, sell_concurrency
                 FROM tools ORDER BY provider, account_id",
            )
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(provider, account_id, keychain_ref, plan, origin, source, sources, sell_enabled, sell_daily_limit,
                  sell_min_ratio, sell_max_ratio, sell_concurrency)| {
                    // Rows written before the `sources` column existed carry
                    // only the single `source`; present that as a one-element
                    // list so callers never special-case the old shape.
                    let sources = sources
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                        .unwrap_or_else(|| source.clone().into_iter().collect());
                    ToolRow {
                        provider,
                        account_id,
                        keychain_ref,
                        plan,
                        origin,
                        source,
                        sources,
                        sell_enabled: sell_enabled != 0,
                        sell_daily_limit,
                        // A row written before the band existed reads as the
                        // full range, never as an empty one: an upgrade must
                        // not silently take a subscription off the market.
                        sell_min_ratio,
                        sell_max_ratio,
                        // Same reading as the band: a row from before the
                        // column existed carries 0, which means "never set"
                        // rather than "serve nothing".
                        sell_concurrency: normalise_concurrency(sell_concurrency),
                    }
                },
            )
            .collect())
    }

    /// Set one account's sell switch and daily token cap (0 = unlimited).
    /// Returns false when the account is not in the store.
    pub async fn set_tool_sell(
        &self,
        provider: &str,
        account_id: &str,
        enabled: bool,
        daily_limit: i64,
    ) -> anyhow::Result<bool> {
        let res = sqlx::query("UPDATE tools SET sell_enabled=?, sell_daily_limit=? WHERE provider=? AND account_id=?")
            .bind(i64::from(enabled))
            .bind(daily_limit.max(0))
            .bind(provider)
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Set one account's price band, in whole percent of list price.
    ///
    /// Stored as a separate statement from `set_tool_sell` because it answers a
    /// different question — *at what price* this subscription is for sale, not
    /// *whether* it is — and the two are edited independently in the UI.
    /// Nonsense input is normalised rather than rejected: the band is clamped
    /// to `RATIO_BAND_FULL` and an inverted pair is swapped, so a half-typed
    /// number can never leave an account with a band nothing can satisfy.
    pub async fn set_tool_ratio_band(
        &self,
        provider: &str,
        account_id: &str,
        min_ratio: i64,
        max_ratio: i64,
    ) -> anyhow::Result<bool> {
        let (lo, hi) = normalise_band(min_ratio, max_ratio);
        let res = sqlx::query("UPDATE tools SET sell_min_ratio=?, sell_max_ratio=? WHERE provider=? AND account_id=?")
            .bind(lo)
            .bind(hi)
            .bind(provider)
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Set how many requests one account serves at once.
    ///
    /// Its own statement for the same reason the band is: it answers "how much
    /// of this subscription is for sale", which the operator edits separately
    /// from whether it sells at all and from what it will accept for it. The
    /// value is normalised rather than rejected, so a half-typed number lands
    /// on the default instead of leaving a lane the market cannot pick.
    pub async fn set_tool_concurrency(
        &self,
        provider: &str,
        account_id: &str,
        concurrency: i64,
    ) -> anyhow::Result<bool> {
        let res = sqlx::query("UPDATE tools SET sell_concurrency=? WHERE provider=? AND account_id=?")
            .bind(normalise_concurrency(concurrency))
            .bind(provider)
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Insert (or replace) a provider-side task metering record (spec §8).
    /// `provider`/`account_id` attribute the usage to the exact subscription
    /// account that served it, so per-account sell limits and window estimates
    /// never mix two accounts of the same provider.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_provider_record(
        &self,
        task_id: &str,
        provider: &str,
        account_id: &str,
        model: &str,
        in_tokens: i64,
        out_tokens: i64,
        cache_read: i64,
        cache_write: i64,
        status: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO provider_records(task_id, ts, provider, account_id, model, in_tokens, out_tokens, cache_read, cache_write, amount_usdt, counterparty_anon, status)
             VALUES(?, strftime('%s','now'), ?, ?, ?, ?, ?, ?, ?, 0, '', ?)
             ON CONFLICT(task_id) DO UPDATE SET
               provider=excluded.provider, account_id=excluded.account_id,
               in_tokens=excluded.in_tokens, out_tokens=excluded.out_tokens,
               cache_read=excluded.cache_read, cache_write=excluded.cache_write, status=excluded.status",
        )
        .bind(task_id)
        .bind(provider)
        .bind(account_id)
        .bind(model)
        .bind(in_tokens)
        .bind(out_tokens)
        .bind(cache_read)
        .bind(cache_write)
        .bind(status)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Tokens (in+out) this exact account served in the last `window_secs`.
    /// Rows written before per-account attribution existed carry an empty
    /// `account_id` and are therefore excluded — they age out of the window.
    pub async fn served_tokens_since_for_account(
        &self,
        window_secs: i64,
        provider: &str,
        account_id: &str,
    ) -> anyhow::Result<u64> {
        let sql = format!(
            "SELECT COALESCE(SUM(in_tokens + out_tokens), 0) FROM provider_records
             WHERE ts >= (strftime('%s','now') - {window_secs}) AND provider=? AND account_id=?"
        );
        let total: (i64,) = sqlx::query_as(&sql)
            .bind(provider)
            .bind(account_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(total.0.max(0) as u64)
    }

    /// Tokens (in+out) this exact account served since the start of the current
    /// UTC day — drives the per-account daily sell cap / auto-stop.
    pub async fn served_tokens_today_for_account(&self, provider: &str, account_id: &str) -> anyhow::Result<u64> {
        let total: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(in_tokens + out_tokens), 0) FROM provider_records
             WHERE ts >= strftime('%s','now','start of day') AND provider=? AND account_id=?",
        )
        .bind(provider)
        .bind(account_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(total.0.max(0) as u64)
    }

    /// Sum tokens (in+out) served as a publisher in the last `window_secs`,
    /// optionally filtered to models whose name starts with `model_prefix`.
    /// Drives the real rolling-window quota estimate (§P0-1).
    pub async fn served_tokens_since(&self, window_secs: i64, model_prefix: Option<&str>) -> anyhow::Result<u64> {
        let cutoff = format!("strftime('%s','now') - {window_secs}");
        let sql = format!(
            "SELECT COALESCE(SUM(in_tokens + out_tokens), 0) FROM provider_records WHERE ts >= ({cutoff})"
        );
        let total: (i64,) = if let Some(prefix) = model_prefix {
            sqlx::query_as(&format!("{sql} AND model LIKE ?"))
                .bind(format!("{prefix}%"))
                .fetch_one(&self.pool)
                .await?
        } else {
            sqlx::query_as(&sql).fetch_one(&self.pool).await?
        };
        Ok(total.0.max(0) as u64)
    }

    /// Sum tokens (in+out) served as a publisher since the start of the current
    /// UTC day, optionally filtered by model prefix. Drives the per-day publish
    /// limit / auto-stop (flow §3).
    pub async fn served_tokens_today(&self, model_prefix: Option<&str>) -> anyhow::Result<u64> {
        let sql = "SELECT COALESCE(SUM(in_tokens + out_tokens), 0) FROM provider_records \
                   WHERE ts >= strftime('%s','now','start of day')";
        let total: (i64,) = if let Some(prefix) = model_prefix {
            sqlx::query_as(&format!("{sql} AND model LIKE ?"))
                .bind(format!("{prefix}%"))
                .fetch_one(&self.pool)
                .await?
        } else {
            sqlx::query_as(sql).fetch_one(&self.pool).await?
        };
        Ok(total.0.max(0) as u64)
    }

    /// Record a tool credential reference (keychain_ref only; no plaintext).
    /// `origin` is `oauth` or `api_key` (asale holds this credential
    /// exclusively) or `import` (copied from a locally installed CLI, so its
    /// refresh token is shared with it). Re-importing never downgrades an
    /// exclusively-held account to `import` — that flag drives a shared-rotation
    /// warning which would then be shown about a credential no CLI can rotate —
    /// and the per-account sell switch/limit survive a re-import.
    ///
    /// `source` is protected by the same rule, because it names the credential
    /// actually in use: letting a re-import relabel an asale-held account with
    /// the CLI's file path would show a provenance that contradicts `origin`.
    ///
    /// `sources` is every local store found holding *this same subscription
    /// account*, best first — the row stays one-per-account, and the extra
    /// entries are informational (the Sell page lists them). Those *are* updated
    /// on a re-import: which stores hold this account is a fact about the
    /// machine, not about the credential asale uses. An empty slice is treated
    /// as an unknown source.
    pub async fn upsert_tool(
        &self,
        provider: &str,
        account_id: &str,
        keychain_ref: &str,
        sources: &[&str],
        origin: &str,
    ) -> anyhow::Result<()> {
        let primary = sources.first().copied().unwrap_or("");
        let all = serde_json::to_string(sources)?;
        sqlx::query(
            "INSERT INTO tools(provider, account_id, keychain_ref, source, sources, origin, discovered_at)
             VALUES(?,?,?,?,?,?,strftime('%s','now'))
             ON CONFLICT(provider, account_id) DO UPDATE SET
               keychain_ref=excluded.keychain_ref,
               source=CASE WHEN tools.origin IN ('oauth','api_key') THEN tools.source ELSE excluded.source END,
               sources=excluded.sources,
               origin=CASE WHEN tools.origin IN ('oauth','api_key') THEN tools.origin ELSE excluded.origin END",
        )
        .bind(provider)
        .bind(account_id)
        .bind(keychain_ref)
        .bind(primary)
        .bind(all)
        .bind(origin)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove an imported account row (spec §4 `remove_account`). The caller
    /// deletes the keychain entries the row referenced.
    pub async fn delete_tool(&self, provider: &str, account_id: &str) -> anyhow::Result<bool> {
        let res = sqlx::query("DELETE FROM tools WHERE provider=? AND account_id=?")
            .bind(provider)
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Fold one call this machine served for itself into the `used` snapshot —
    /// the same bucket the CLI log scanner writes to, under today's UTC day.
    ///
    /// This is the direct route: the local proxy answering a tool from an
    /// imported subscription, with no trade and so no price. It is counted here
    /// rather than kept as its own row because "我使用的" is exactly the question
    /// it answers — whose subscription was spent, not what anything cost — and a
    /// separate table for it was a category nobody was shown.
    ///
    /// The caller decides *whether* to fold: a tool whose own session logs the
    /// scanner already reads would otherwise be counted twice. See
    /// `usage_scan::scanner_covers`.
    pub async fn record_local_usage(
        &self,
        model: &str,
        in_tokens: i64,
        out_tokens: i64,
        cache: i64,
    ) -> anyhow::Result<()> {
        // `now`, not the response's own timestamp: a direct call is metered as
        // its stream ends, which is the same instant either way.
        let day: (String,) = sqlx::query_as("SELECT strftime('%Y-%m-%d','now')")
            .fetch_one(&self.pool)
            .await?;
        self.add_usage("used", &day.0, model, in_tokens.max(0), out_tokens.max(0), cache.max(0), 1, 0)
            .await
    }

    /// Paged read of provider (publisher-side) records, newest first.
    pub async fn list_provider_records(&self, limit: i64, offset: i64) -> anyhow::Result<(Vec<RecordRow>, i64)> {
        let rows: Vec<(String, i64, String, i64, i64, i64, i64, i64, String)> = sqlx::query_as(
            "SELECT task_id, ts, model, in_tokens, out_tokens, cache_read, cache_write, amount_usdt, status
             FROM provider_records ORDER BY ts DESC, task_id DESC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM provider_records").fetch_one(&self.pool).await?;
        let out = rows
            .into_iter()
            .map(|(task_id, ts, model, in_tokens, out_tokens, cache_read, cache_write, amount_usdt, status)| RecordRow {
                task_id,
                ts,
                model,
                in_tokens,
                out_tokens,
                cache_read,
                cache_write,
                amount_usdt,
                status,
            })
            .collect();
        Ok((out, total.0))
    }


    /// Update a provider record's settled amount from the server ledger
    /// (reconcile, spec §8.1 — server is authoritative).
    pub async fn set_provider_record_amount(&self, task_id: &str, amount_usdt: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE provider_records SET amount_usdt=? WHERE task_id=?")
            .bind(amount_usdt)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Aggregate provider-side (sold) usage since `since_ts` (unix seconds; None
    /// = all-time): `(tokens = in+out, amount_usdt = earnings, count)`. Drives
    /// the "我卖的" category on the usage page.
    pub async fn sold_summary(&self, since_ts: Option<i64>) -> anyhow::Result<(i64, i64, i64)> {
        summary_over(&self.pool, "provider_records", since_ts).await
    }

    /// Earliest provider-record timestamp within the last `window_secs`,
    /// optionally filtered by model prefix. Used to estimate when a rolling
    /// window's oldest usage ages out (the window "reset" for the limits view).
    pub async fn oldest_served_ts_since(&self, window_secs: i64, model_prefix: Option<&str>) -> anyhow::Result<Option<i64>> {
        let cutoff = format!("strftime('%s','now') - {window_secs}");
        let sql = format!("SELECT MIN(ts) FROM provider_records WHERE ts >= ({cutoff})");
        let row: (Option<i64>,) = if let Some(prefix) = model_prefix {
            sqlx::query_as(&format!("{sql} AND model LIKE ?"))
                .bind(format!("{prefix}%"))
                .fetch_one(&self.pool)
                .await?
        } else {
            sqlx::query_as(&sql).fetch_one(&self.pool).await?
        };
        Ok(row.0)
    }

}

/// A normalized `SELECT` over one records table exposing a common column set
/// (`ts, model, in_tokens, out_tokens, cache, amount_usdt`). Table names are
/// trusted constants.
///
/// One table, for now. The shape is kept because it is what lets a caller ask
/// one question of several ledgers, and `provider_records` stopped being the
/// only one once before.
fn norm_select(table: &str) -> Option<&'static str> {
    match table {
        "provider_records" => Some("SELECT ts, model, in_tokens, out_tokens, (cache_read + cache_write) AS cache, amount_usdt FROM provider_records"),
        _ => None,
    }
}

/// `(SELECT … UNION ALL SELECT …)` over the requested tables, as a subquery.
fn union_from(tables: &[&str]) -> String {
    let parts: Vec<&str> = tables.iter().filter_map(|t| norm_select(t)).collect();
    format!("({})", parts.join(" UNION ALL "))
}

fn since_clause(since_ts: Option<i64>) -> String {
    match since_ts {
        Some(_) => "WHERE ts >= ?".into(),
        None => String::new(),
    }
}

impl LocalStore {
    /// Totals over the given tables since `since_ts`: `(tokens, amount, count)`.
    pub async fn usage_totals(&self, tables: &[&str], since_ts: Option<i64>) -> anyhow::Result<(i64, i64, i64)> {
        let sql = format!(
            "SELECT COALESCE(SUM(in_tokens + out_tokens),0), COALESCE(SUM(amount_usdt),0), COUNT(*) FROM {} t {}",
            union_from(tables), since_clause(since_ts)
        );
        let mut q = sqlx::query_as(&sql);
        if let Some(ts) = since_ts { q = q.bind(ts); }
        Ok(q.fetch_one(&self.pool).await?)
    }

    /// Per-model breakdown `(model, tokens, count)`, newest-first by tokens.
    pub async fn usage_by_model(&self, tables: &[&str], since_ts: Option<i64>, limit: i64) -> anyhow::Result<Vec<(String, i64, i64)>> {
        let sql = format!(
            "SELECT model, COALESCE(SUM(in_tokens + out_tokens),0) AS tk, COUNT(*) FROM {} t {} GROUP BY model ORDER BY tk DESC LIMIT ?",
            union_from(tables), since_clause(since_ts)
        );
        let mut q = sqlx::query_as(&sql);
        if let Some(ts) = since_ts { q = q.bind(ts); }
        Ok(q.bind(limit).fetch_all(&self.pool).await?)
    }

    /// Per-UTC-day breakdown `(date, total, input, output, cache, count)`, oldest-first.
    pub async fn usage_by_day(&self, tables: &[&str], since_ts: Option<i64>) -> anyhow::Result<Vec<(String, i64, i64, i64, i64, i64)>> {
        let sql = format!(
            "SELECT strftime('%Y-%m-%d', ts, 'unixepoch') AS d, \
             COALESCE(SUM(in_tokens + out_tokens),0), COALESCE(SUM(in_tokens),0), \
             COALESCE(SUM(out_tokens),0), COALESCE(SUM(cache),0), COUNT(*) \
             FROM {} t {} GROUP BY d ORDER BY d",
            union_from(tables), since_clause(since_ts)
        );
        let mut q = sqlx::query_as(&sql);
        if let Some(ts) = since_ts { q = q.bind(ts); }
        Ok(q.fetch_all(&self.pool).await?)
    }

    /// `(distinct active UTC days, earliest ts)` across the given tables.
    pub async fn usage_active(&self, tables: &[&str]) -> anyhow::Result<(i64, Option<i64>)> {
        let sql = format!(
            "SELECT COUNT(DISTINCT strftime('%Y-%m-%d', ts, 'unixepoch')), MIN(ts) FROM {} t",
            union_from(tables)
        );
        Ok(sqlx::query_as(&sql).fetch_one(&self.pool).await?)
    }

    // ── Incremental usage snapshot (`usage_daily`) ──────────────────────────

    /// Fold newly-inserted ledger rows into the `usage_daily` snapshot. Each
    /// source table keeps a rowid cursor in `settings`, so a run only scans rows
    /// appended since last time — never the full ledger. Idempotent; safe to run
    /// on a timer and on demand. Returns the number of rows folded in.
    pub async fn aggregate_usage(&self) -> anyhow::Result<u64> {
        let mut folded = 0u64;
        // Sell side only. Market purchases are the server's to report
        // (`/me/usage`) — the client never learns what one cost — and direct
        // calls fold themselves in as `used` when they happen, via
        // `record_local_usage`, since there is no table left to sweep.
        for (table, source, cache_expr) in [("provider_records", "sold", "(cache_read + cache_write)")] {
            let cursor_key = format!("usage_agg_rowid:{table}");
            let cursor: i64 = self
                .get_setting(&cursor_key)
                .await?
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let sql = format!(
                "SELECT rowid, strftime('%Y-%m-%d', ts, 'unixepoch') AS day, model, \
                 in_tokens, out_tokens, {cache_expr} AS cache, amount_usdt \
                 FROM {table} WHERE rowid > ? ORDER BY rowid"
            );
            let rows: Vec<(i64, String, String, i64, i64, i64, i64)> =
                sqlx::query_as(&sql).bind(cursor).fetch_all(&self.pool).await?;
            if rows.is_empty() {
                continue;
            }
            let mut max_rowid = cursor;
            let mut tx = self.pool.begin().await?;
            for (rowid, day, model, in_tokens, out_tokens, cache, amount) in rows {
                sqlx::query(
                    "INSERT INTO usage_daily(source, day, model, in_tokens, out_tokens, cache, cnt, amount)
                     VALUES(?,?,?,?,?,?,1,?)
                     ON CONFLICT(source, day, model) DO UPDATE SET
                       in_tokens = in_tokens + excluded.in_tokens,
                       out_tokens = out_tokens + excluded.out_tokens,
                       cache = cache + excluded.cache,
                       cnt = cnt + 1,
                       amount = amount + excluded.amount",
                )
                .bind(source).bind(&day).bind(&model)
                .bind(in_tokens).bind(out_tokens).bind(cache).bind(amount)
                .execute(&mut *tx).await?;
                if rowid > max_rowid { max_rowid = rowid; }
                folded += 1;
            }
            sqlx::query("INSERT INTO settings(k,v) VALUES(?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v")
                .bind(&cursor_key).bind(max_rowid.to_string())
                .execute(&mut *tx).await?;
            tx.commit().await?;
        }
        Ok(folded)
    }

    /// Add a bucket of usage into the snapshot (used by the local-CLI-log
    /// scanner for `source="used"`; ledger sources use `aggregate_usage`).
    pub async fn add_usage(&self, source: &str, day: &str, model: &str, in_tokens: i64, out_tokens: i64, cache: i64, cnt: i64, amount: i64) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO usage_daily(source, day, model, in_tokens, out_tokens, cache, cnt, amount)
             VALUES(?,?,?,?,?,?,?,?)
             ON CONFLICT(source, day, model) DO UPDATE SET
               in_tokens = in_tokens + excluded.in_tokens,
               out_tokens = out_tokens + excluded.out_tokens,
               cache = cache + excluded.cache,
               cnt = cnt + excluded.cnt,
               amount = amount + excluded.amount",
        )
        .bind(source).bind(day).bind(model).bind(in_tokens).bind(out_tokens).bind(cache).bind(cnt).bind(amount)
        .execute(&self.pool).await?;
        Ok(())
    }

    /// Last-parsed `(byte_offset, mtime)` for a CLI log file — `(0, 0)` if new.
    pub async fn get_scan_offset(&self, path: &str) -> anyhow::Result<(i64, i64)> {
        let row: Option<(i64, i64)> = sqlx::query_as("SELECT byte_offset, mtime FROM usage_scan WHERE path=?")
            .bind(path).fetch_optional(&self.pool).await?;
        Ok(row.unwrap_or((0, 0)))
    }

    /// Persist a CLI log file's parse cursor.
    pub async fn set_scan_offset(&self, path: &str, byte_offset: i64, mtime: i64) -> anyhow::Result<()> {
        sqlx::query("INSERT OR REPLACE INTO usage_scan(path, byte_offset, mtime) VALUES(?,?,?)")
            .bind(path).bind(byte_offset).bind(mtime).execute(&self.pool).await?;
        Ok(())
    }

    /// Totals from the snapshot: `(tokens, amount, count)`.
    pub async fn agg_totals(&self, sources: &[&str], since_day: Option<&str>) -> anyhow::Result<(i64, i64, i64)> {
        let mut sql = format!(
            "SELECT COALESCE(SUM(in_tokens + out_tokens),0), COALESCE(SUM(amount),0), COALESCE(SUM(cnt),0) FROM usage_daily WHERE {}",
            source_in(sources)
        );
        if since_day.is_some() { sql.push_str(" AND day >= ?"); }
        let mut q = sqlx::query_as(&sql);
        if let Some(d) = since_day { q = q.bind(d); }
        Ok(q.fetch_one(&self.pool).await?)
    }

    /// Per-model breakdown from the snapshot `(model, tokens, count)`.
    pub async fn agg_by_model(&self, sources: &[&str], since_day: Option<&str>, limit: i64) -> anyhow::Result<Vec<(String, i64, i64)>> {
        let mut sql = format!(
            "SELECT model, COALESCE(SUM(in_tokens + out_tokens),0) AS tk, COALESCE(SUM(cnt),0) FROM usage_daily WHERE {}",
            source_in(sources)
        );
        if since_day.is_some() { sql.push_str(" AND day >= ?"); }
        sql.push_str(" GROUP BY model ORDER BY tk DESC LIMIT ?");
        let mut q = sqlx::query_as(&sql);
        if let Some(d) = since_day { q = q.bind(d); }
        Ok(q.bind(limit).fetch_all(&self.pool).await?)
    }

    /// Per-day rows from the snapshot `(day, total, input, output, cache, count)`.
    pub async fn agg_by_day(&self, sources: &[&str], since_day: Option<&str>) -> anyhow::Result<Vec<(String, i64, i64, i64, i64, i64)>> {
        let mut sql = format!(
            "SELECT day, COALESCE(SUM(in_tokens + out_tokens),0), COALESCE(SUM(in_tokens),0), \
             COALESCE(SUM(out_tokens),0), COALESCE(SUM(cache),0), COALESCE(SUM(cnt),0) \
             FROM usage_daily WHERE {}",
            source_in(sources)
        );
        if since_day.is_some() { sql.push_str(" AND day >= ?"); }
        sql.push_str(" GROUP BY day ORDER BY day");
        let mut q = sqlx::query_as(&sql);
        if let Some(d) = since_day { q = q.bind(d); }
        Ok(q.fetch_all(&self.pool).await?)
    }

    /// `(distinct active days, earliest day)` from the snapshot.
    pub async fn agg_active(&self, sources: &[&str]) -> anyhow::Result<(i64, Option<String>)> {
        let sql = format!("SELECT COUNT(DISTINCT day), MIN(day) FROM usage_daily WHERE {}", source_in(sources));
        Ok(sqlx::query_as(&sql).fetch_one(&self.pool).await?)
    }
}

/// `source IN ('sold','bought')` for the requested snapshot sources. Values are
/// trusted constants, never user input.
fn source_in(sources: &[&str]) -> String {
    let quoted: Vec<String> = sources.iter().map(|s| format!("'{s}'")).collect();
    format!("source IN ({})", quoted.join(","))
}

/// Sum `(in+out tokens, amount_usdt, row count)` for a records table, optionally
/// filtered to rows at/after `since_ts`. `table` is a trusted constant (never
/// user input), so interpolating it into the SQL is safe.
async fn summary_over(
    pool: &sqlx::SqlitePool,
    table: &str,
    since_ts: Option<i64>,
) -> anyhow::Result<(i64, i64, i64)> {
    let base = format!(
        "SELECT COALESCE(SUM(in_tokens + out_tokens), 0), COALESCE(SUM(amount_usdt), 0), COUNT(*) FROM {table}"
    );
    let row: (i64, i64, i64) = if let Some(ts) = since_ts {
        sqlx::query_as(&format!("{base} WHERE ts >= ?")).bind(ts).fetch_one(pool).await?
    } else {
        sqlx::query_as(&base).fetch_one(pool).await?
    };
    Ok(row)
}

/// A local task record row (both tables; consume rows have zero cache fields).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordRow {
    pub task_id: String,
    pub ts: i64,
    pub model: String,
    pub in_tokens: i64,
    pub out_tokens: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub amount_usdt: i64,
    pub status: String,
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_unset_concurrency_reads_as_the_default_not_as_one() {
        use super::{normalise_concurrency, DEFAULT_SELL_CONCURRENCY, SELL_CONCURRENCY_RANGE};
        // A row written before the column existed, and a half-typed input, both
        // arrive as 0. Answering that with the floor would quietly take four
        // fifths of an upgraded seller's capacity off the market.
        assert_eq!(normalise_concurrency(0), DEFAULT_SELL_CONCURRENCY);
        assert_eq!(normalise_concurrency(-3), DEFAULT_SELL_CONCURRENCY);
        // Deliberate values are kept; absurd ones are clamped rather than
        // rejected, so a typo cannot leave an account unable to sell.
        assert_eq!(normalise_concurrency(1), 1);
        assert_eq!(normalise_concurrency(12), 12);
        assert_eq!(normalise_concurrency(9_999), SELL_CONCURRENCY_RANGE.1);
    }

    use super::*;

    #[tokio::test]
    async fn schema_and_settings_roundtrip() {
        let s = LocalStore::open_memory().await.unwrap();
        s.set_setting("proxy_port", "9787").await.unwrap();
        assert_eq!(s.get_setting("proxy_port").await.unwrap().as_deref(), Some("9787"));
        s.upsert_tool("claude", "a@b.io", "asale:claude:a@b.io", &["oauth"], "oauth").await.unwrap();
        // No plaintext credential columns exist — only keychain_ref.
        let tools = s.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].provider, "claude");
    }

    /// A CLI re-import must not relabel an account asale logged into itself.
    ///
    /// The daemon re-imports local CLI credentials on every start, and the same
    /// subscription is one row whichever way it was found — so this upsert is
    /// what stands between "asale holds this exclusively" and a row that says so
    /// while pointing at the CLI's file. The extra stores holding the account
    /// still get recorded; that is a fact about the machine, not the credential.
    #[tokio::test]
    async fn a_reimport_cannot_relabel_an_exclusively_held_account() {
        let s = LocalStore::open_memory().await.unwrap();
        s.upsert_tool("claude", "a@b.io", "ref", &["oauth"], "oauth").await.unwrap();
        s.upsert_tool("claude", "a@b.io", "ref", &[".claude/.credentials.json"], "import").await.unwrap();

        let tools = s.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1, "one subscription is one row");
        assert_eq!(tools[0].origin.as_deref(), Some("oauth"), "origin survives the re-import");
        assert_eq!(tools[0].source.as_deref(), Some("oauth"), "and so does the provenance shown for it");
        assert_eq!(tools[0].sources, [".claude/.credentials.json"], "but the CLI store is still recorded");

        // An imported account, on the other hand, is still free to be upgraded
        // once the user logs in through asale itself.
        s.upsert_tool("codex", "c@d.io", "ref2", &[".codex/auth.json"], "import").await.unwrap();
        s.upsert_tool("codex", "c@d.io", "ref2", &["oauth"], "oauth").await.unwrap();
        let codex = s.list_tools().await.unwrap().into_iter().find(|t| t.provider == "codex").unwrap();
        assert_eq!(codex.origin.as_deref(), Some("oauth"));
        assert_eq!(codex.source.as_deref(), Some("oauth"));
    }

    /// A database created before `provider`/`account_id` existed must still open:
    /// the schema step may not reference columns that only `MIGRATIONS` adds.
    #[tokio::test]
    async fn opens_pre_migration_database() {
        let dir = std::env::temp_dir().join(format!("asale-store-mig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        let _ = std::fs::remove_file(&path);
        let db = path.to_str().unwrap();

        // Shape of the pre-migration tables (no origin/sell_*/provider/account_id).
        {
            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{db}"))
                .unwrap()
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
            sqlx::query(
                "CREATE TABLE tools (id INTEGER PRIMARY KEY, provider TEXT NOT NULL, account_id TEXT NOT NULL,
                   keychain_ref TEXT NOT NULL, plan TEXT, source TEXT, discovered_at INTEGER,
                   UNIQUE(provider, account_id));
                 CREATE TABLE provider_records (task_id TEXT PRIMARY KEY, ts INTEGER, model TEXT,
                   in_tokens INTEGER, out_tokens INTEGER, cache_read INTEGER, cache_write INTEGER,
                   amount_usdt INTEGER, counterparty_anon TEXT, status TEXT);",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let s = LocalStore::open(db).await.expect("old database must migrate, not fail");
        s.upsert_tool("claude", "a@b.io", "asale:claude:a@b.io", &["oauth"], "oauth").await.unwrap();
        assert!(!s.list_tools().await.unwrap().is_empty());
        s.insert_provider_record("p1", "claude", "a@b.io", "claude-sonnet", 1, 1, 0, 0, "ok").await.unwrap();
        assert_eq!(s.served_tokens_today_for_account("claude", "a@b.io").await.unwrap(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn buy_switch_updates_only_the_fields_it_is_given() {
        let s = LocalStore::open_memory().await.unwrap();
        assert_eq!(s.buy_tool("claude").await.unwrap(), BuyToolRow::off("claude"));

        s.set_buy_tool("claude", Some(true), Some(&["m1".into()]), Some("{}"), Some(99))
            .await
            .unwrap();
        // Editing the model selection must not disturb the switch, the stored
        // backup or the "buying since" date.
        s.set_buy_tool("claude", None, Some(&["m1".into(), "m2".into()]), None, None)
            .await
            .unwrap();

        let row = s.buy_tool("claude").await.unwrap();
        assert!(row.enabled);
        assert_eq!(row.models, ["m1", "m2"]);
        assert_eq!(row.backup_json.as_deref(), Some("{}"));
        assert_eq!(row.since_ts, 99);

        // Tools are independent of one another.
        assert_eq!(s.buy_tool("codex").await.unwrap(), BuyToolRow::off("codex"));
    }

    #[tokio::test]
    async fn buy_switches_carry_over_from_the_old_composed_settings_keys() {
        let dir = std::env::temp_dir().join(format!("asale-store-buy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kv.db");
        let _ = std::fs::remove_file(&path);
        let db = path.to_str().unwrap();

        // An install from before `buy_tools` existed: four settings rows per tool.
        {
            let s = LocalStore::open(db).await.unwrap();
            sqlx::query("DELETE FROM buy_tools").execute(&s.pool).await.unwrap();
            for (k, v) in [
                ("buy_enabled:claude", "1"),
                ("buy_models:claude", r#"["claude-opus-5"]"#),
                ("buy_backup:claude", r#"{"tool":"claude","files":[]}"#),
                ("buy_since:claude", "1700000000"),
                ("buy_enabled:codex", "0"),
            ] {
                s.set_setting(k, v).await.unwrap();
            }
            s.pool.close().await;
        }

        let s = LocalStore::open(db).await.unwrap();
        let claude = s.buy_tool("claude").await.unwrap();
        assert!(claude.enabled, "an install that was buying must still be buying");
        assert_eq!(claude.models, ["claude-opus-5"]);
        assert_eq!(claude.backup_json.as_deref(), Some(r#"{"tool":"claude","files":[]}"#));
        assert_eq!(claude.since_ts, 1_700_000_000);
        assert!(!s.buy_tool("codex").await.unwrap().enabled);

        // The old keys are gone, so nothing can read them back by accident.
        assert_eq!(s.get_setting("buy_enabled:claude").await.unwrap(), None);
        assert_eq!(s.get_setting("buy_models:claude").await.unwrap(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A direct call folds into the same `used` bucket the CLI log scanner
    /// writes to, so the two are one number rather than two sources the page
    /// has to choose between.
    #[tokio::test]
    async fn a_direct_call_lands_in_the_used_bucket() {
        let s = LocalStore::open_memory().await.unwrap();
        s.record_local_usage("claude-sonnet", 10, 5, 3).await.unwrap();
        s.record_local_usage("claude-sonnet", 20, 6, 0).await.unwrap();
        // Same day and model: the two calls add up rather than replacing.
        let (tokens, amount, count) = s.agg_totals(&["used"], None).await.unwrap();
        assert_eq!(tokens, 41, "10+5 then 20+6, cache counted beside the total");
        assert_eq!(amount, 0, "a direct call is the operator's own subscription — no trade, no price");
        assert_eq!(count, 2);

        let daily = s.agg_by_day(&["used"], None).await.unwrap();
        assert_eq!(daily.len(), 1);
        let (_day, total, input, output, cache, cnt) = &daily[0];
        assert_eq!((*total, *input, *output, *cache, *cnt), (41, 30, 11, 3, 2));

        // It is not the sell side, and `aggregate_usage` has nothing to sweep
        // for it: the fold already happened when the call did.
        assert_eq!(s.agg_totals(&["sold"], None).await.unwrap().0, 0);
        assert_eq!(s.aggregate_usage().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn provider_records_paging_and_tool_delete() {
        let s = LocalStore::open_memory().await.unwrap();
        // Provider record amount sync (reconcile).
        s.insert_provider_record("p1", "claude", "a@b.io", "claude-sonnet", 100, 50, 0, 0, "ok").await.unwrap();
        s.set_provider_record_amount("p1", 777).await.unwrap();
        let (prow, ptotal) = s.list_provider_records(10, 0).await.unwrap();
        assert_eq!(ptotal, 1);
        assert_eq!(prow[0].amount_usdt, 777);

        // Tool delete.
        s.upsert_tool("claude", "a@b.io", "claude:a@b.io", &["keychain"], "import").await.unwrap();
        assert!(s.delete_tool("claude", "a@b.io").await.unwrap());
        assert!(!s.delete_tool("claude", "a@b.io").await.unwrap());
        assert!(s.list_tools().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_records_and_window_sum() {
        let s = LocalStore::open_memory().await.unwrap();
        s.insert_provider_record("t1", "claude", "a@b.io", "claude-sonnet", 100, 50, 0, 0, "ok").await.unwrap();
        s.insert_provider_record("t2", "gemini", "g@b.io", "gemini-2.5-pro", 10, 5, 0, 0, "ok").await.unwrap();
        // All-model window sum.
        assert_eq!(s.served_tokens_since(18_000, None).await.unwrap(), 165);
        // Prefix-filtered (claude only).
        assert_eq!(s.served_tokens_since(18_000, Some("claude")).await.unwrap(), 150);
        // Upsert updates existing row rather than duplicating.
        s.insert_provider_record("t1", "claude", "a@b.io", "claude-sonnet", 200, 50, 0, 0, "ok").await.unwrap();
        assert_eq!(s.served_tokens_since(18_000, Some("claude")).await.unwrap(), 250);
    }

    #[tokio::test]
    async fn usage_snapshot_aggregates_incrementally() {
        let s = LocalStore::open_memory().await.unwrap();
        // Two sold records plus a direct call. Only the sold ones are swept
        // from a table; the direct one folded itself in as `used` when it
        // happened, and market purchases have no local row at all — only the
        // server knows what one cost.
        s.insert_provider_record("p1", "claude", "a@b.io", "claude-opus-4-8", 100, 40, 5, 5, "ok").await.unwrap(); // sold: 140 tok, 10 cache
        s.insert_provider_record("p2", "claude", "a@b.io", "claude-sonnet-5", 20, 10, 0, 0, "ok").await.unwrap();   // sold: 30 tok
        s.record_local_usage("claude-opus-4-8", 60, 30, 0).await.unwrap();                                          // used: 90 tok

        // First fold: the two sold rows. The direct call is already in.
        assert_eq!(s.aggregate_usage().await.unwrap(), 2);
        // A second fold with no new rows folds nothing (cursor advanced).
        assert_eq!(s.aggregate_usage().await.unwrap(), 0);

        // Totals per scope from the snapshot.
        assert_eq!(s.agg_totals(&["sold"], None).await.unwrap().0, 170);      // 140 + 30
        assert_eq!(s.agg_totals(&["used"], None).await.unwrap(), (90, 0, 1)); // tokens, amount, count
        assert_eq!(s.agg_totals(&["sold", "used"], None).await.unwrap().0, 260); // 170 + 90

        // Model breakdown (sold+used): opus = 140 + 90 = 230, sonnet = 30.
        let models = s.agg_by_model(&["sold", "used"], None, 10).await.unwrap();
        assert_eq!(models[0], ("claude-opus-4-8".into(), 230, 2));
        assert_eq!(models[1], ("claude-sonnet-5".into(), 30, 1));

        // Per-day row carries the split columns (single UTC day in the test).
        let daily = s.agg_by_day(&["sold"], None).await.unwrap();
        assert_eq!(daily.len(), 1);
        let (_day, total, input, output, cache, cnt) = &daily[0];
        assert_eq!((*total, *input, *output, *cache, *cnt), (170, 120, 50, 10, 2));

        // Incremental: a new sold row folds by itself and updates the snapshot.
        s.insert_provider_record("p3", "claude", "a@b.io", "claude-opus-4-8", 5, 5, 0, 0, "ok").await.unwrap();
        assert_eq!(s.aggregate_usage().await.unwrap(), 1);
        assert_eq!(s.agg_totals(&["sold"], None).await.unwrap().0, 180); // 170 + 10

        let (active_days, first_day) = s.agg_active(&["sold"]).await.unwrap();
        assert_eq!(active_days, 1);
        assert!(first_day.is_some());
    }
}
