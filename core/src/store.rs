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
CREATE TABLE IF NOT EXISTS consume_records (
  task_id TEXT PRIMARY KEY, ts INTEGER, model TEXT,
  in_tokens INTEGER, out_tokens INTEGER, amount_usdt INTEGER, status TEXT
);
CREATE TABLE IF NOT EXISTS wallet_txns (
  id INTEGER PRIMARY KEY, ts INTEGER, type TEXT,
  amount_usdt INTEGER, tx_hash TEXT, status TEXT
);
CREATE TABLE IF NOT EXISTS settings (k TEXT PRIMARY KEY, v TEXT);
CREATE TABLE IF NOT EXISTS usage_daily (
  source TEXT NOT NULL,          -- 'used' (local CLI logs) | 'sold' | 'bought'
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
        let rows: Vec<(String, String, String, Option<String>, Option<String>, Option<String>, Option<String>, i64, i64)> =
            sqlx::query_as(
                "SELECT provider, account_id, keychain_ref, plan, origin, source, sources, sell_enabled, sell_daily_limit
                 FROM tools ORDER BY provider, account_id",
            )
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(provider, account_id, keychain_ref, plan, origin, source, sources, sell_enabled, sell_daily_limit)| {
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
    /// `sources` is every local store found holding *this same subscription
    /// account*, best first — the row stays one-per-account, and the extra
    /// entries are informational (the Sell page lists them). An empty slice is
    /// treated as an unknown source.
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
               source=excluded.source,
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

    /// Insert (or replace) a consumer-side task record (spec §8). Written by the
    /// local proxy when it forwards to the market (or serves direct).
    pub async fn insert_consume_record(
        &self,
        task_id: &str,
        model: &str,
        in_tokens: i64,
        out_tokens: i64,
        amount_usdt: i64,
        status: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO consume_records(task_id, ts, model, in_tokens, out_tokens, amount_usdt, status)
             VALUES(?, strftime('%s','now'), ?, ?, ?, ?, ?)
             ON CONFLICT(task_id) DO UPDATE SET
               in_tokens=excluded.in_tokens, out_tokens=excluded.out_tokens,
               amount_usdt=excluded.amount_usdt, status=excluded.status",
        )
        .bind(task_id)
        .bind(model)
        .bind(in_tokens)
        .bind(out_tokens)
        .bind(amount_usdt)
        .bind(status)
        .execute(&self.pool)
        .await?;
        Ok(())
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

    /// Paged read of consumer-side records, newest first.
    pub async fn list_consume_records(&self, limit: i64, offset: i64) -> anyhow::Result<(Vec<RecordRow>, i64)> {
        let rows: Vec<(String, i64, String, i64, i64, i64, String)> = sqlx::query_as(
            "SELECT task_id, ts, model, in_tokens, out_tokens, amount_usdt, status
             FROM consume_records ORDER BY ts DESC, task_id DESC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM consume_records").fetch_one(&self.pool).await?;
        let out = rows
            .into_iter()
            .map(|(task_id, ts, model, in_tokens, out_tokens, amount_usdt, status)| RecordRow {
                task_id,
                ts,
                model,
                in_tokens,
                out_tokens,
                cache_read: 0,
                cache_write: 0,
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

    /// Aggregate consumer-side (bought) usage since `since_ts` (unix seconds;
    /// None = all-time): `(tokens = in+out, amount_usdt = spend, count)`. Drives
    /// the "我买的" category on the usage page.
    pub async fn bought_summary(&self, since_ts: Option<i64>) -> anyhow::Result<(i64, i64, i64)> {
        summary_over(&self.pool, "consume_records", since_ts).await
    }
}

/// A normalized `SELECT` over one records table exposing a common column set
/// (`ts, model, in_tokens, out_tokens, cache, amount_usdt`). `consume_records`
/// has no cache columns, so it projects `0`. Table names are trusted constants.
fn norm_select(table: &str) -> Option<&'static str> {
    match table {
        "provider_records" => Some("SELECT ts, model, in_tokens, out_tokens, (cache_read + cache_write) AS cache, amount_usdt FROM provider_records"),
        "consume_records" => Some("SELECT ts, model, in_tokens, out_tokens, 0 AS cache, amount_usdt FROM consume_records"),
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
        for (table, source, cache_expr) in [
            ("provider_records", "sold", "(cache_read + cache_write)"),
            ("consume_records", "bought", "0"),
        ] {
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

    #[tokio::test]
    async fn consume_records_paging_and_tool_delete() {
        let s = LocalStore::open_memory().await.unwrap();
        for i in 0..3 {
            s.insert_consume_record(&format!("c{i}"), "claude-sonnet", 10, 5, 0, "ok").await.unwrap();
        }
        let (rows, total) = s.list_consume_records(2, 0).await.unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 2);
        let (rows2, _) = s.list_consume_records(2, 2).await.unwrap();
        assert_eq!(rows2.len(), 1);

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
        // Two sold + one bought record.
        s.insert_provider_record("p1", "claude", "a@b.io", "claude-opus-4-8", 100, 40, 5, 5, "ok").await.unwrap(); // sold: 140 tok, 10 cache
        s.insert_provider_record("p2", "claude", "a@b.io", "claude-sonnet-5", 20, 10, 0, 0, "ok").await.unwrap();   // sold: 30 tok
        s.insert_consume_record("c1", "claude-opus-4-8", 60, 30, 1_000, "ok").await.unwrap();   // bought: 90 tok, 1000 amount

        // First fold: all three rows.
        assert_eq!(s.aggregate_usage().await.unwrap(), 3);
        // A second fold with no new rows folds nothing (cursor advanced).
        assert_eq!(s.aggregate_usage().await.unwrap(), 0);

        // Totals per scope from the snapshot.
        assert_eq!(s.agg_totals(&["sold"], None).await.unwrap().0, 170);           // 140 + 30
        assert_eq!(s.agg_totals(&["bought"], None).await.unwrap(), (90, 1_000, 1)); // tokens, amount, count
        assert_eq!(s.agg_totals(&["sold", "bought"], None).await.unwrap().0, 260);  // 170 + 90

        // Model breakdown (sold+bought): opus = 140 + 90 = 230, sonnet = 30.
        let models = s.agg_by_model(&["sold", "bought"], None, 10).await.unwrap();
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
