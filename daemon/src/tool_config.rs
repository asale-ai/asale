//! Buy-side config switching for the AI CLIs installed on this machine
//! (flow §4/§6), generalized over Claude Code, Codex and Gemini CLI.
//!
//! Turning "buy" on for a tool rewrites that tool's own config so it talks to
//! the asale local proxy instead of the vendor endpoint; turning it off restores
//! the original files verbatim. Each tool keeps its own files, so the three
//! switches are fully independent:
//!
//!   claude → `~/.claude/settings.json`  (`env.ANTHROPIC_BASE_URL` / `…_AUTH_TOKEN`)
//!   codex  → `~/.codex/config.toml` (`model_provider` + `[model_providers.asale]`
//!            + `model` + `model_catalog_json`) and `~/.codex/auth.json`
//!            (`OPENAI_API_KEY`)
//!   gemini → `~/.gemini/.env` (`GOOGLE_GEMINI_BASE_URL` / `GEMINI_API_KEY`)
//!
//! This is the *only* place asale writes into a vendor CLI's directory, and it
//! happens solely on the buy side. The sell side never reads or writes these
//! files — it keeps its own per-account credential copies (see `auth_store`), so
//! selling a subscription can never disturb the CLI you use locally.
//!
//! Writes are atomic (tmp + rename) and the untouched original of every file is
//! captured in a `Backup` before the first write, so a restore is byte-exact.
//! Everything here is blocking filesystem work — call it from `spawn_blocking`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The tools a buy switch can be turned on for.
pub const TOOLS: &[&str] = &["claude", "codex", "gemini", "openclaw", "hermes"];

/// Display name for a tool id.
pub fn label(tool: &str) -> &'static str {
    match tool {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "gemini" => "Gemini CLI",
        "openclaw" => "OpenClaw",
        "hermes" => "Hermes",
        _ => "unknown",
    }
}

/// Reject unknown ids at the API boundary.
pub fn known(tool: &str) -> bool {
    TOOLS.contains(&tool)
}

/// Which locally installed CLI a proxied request came from.
///
/// Two ways, in priority order:
///
///   1. An explicit `/{tool}/…` prefix in the path. This is the only thing that
///      can tell two tools apart at all, and it exists because the dialect
///      cannot: OpenClaw speaks whichever wire format it is configured for, so
///      an OpenClaw request is byte-identical to a Claude Code or Codex one.
///      Inferring the tool from the dialect would gate OpenClaw on *Codex's*
///      switch and hand it Codex's model selection.
///   2. Failing that, the dialect — which is what the three tools that predate
///      the prefix are still addressed by, so their configs keep working.
///
/// Lives here, beside [`TOOLS`], because it is the same knowledge: adding a
/// tool means adding it to both, and having them in one file is what makes that
/// obvious.
pub fn for_request_path(path: &str) -> Option<&'static str> {
    if let Some(tool) = tool_prefix_of(path) {
        return Some(tool);
    }
    if path.starts_with("/v1/messages") {
        Some("claude")
    } else if path.starts_with("/v1beta/") {
        Some("gemini")
    } else if path.starts_with("/v1/chat/completions")
        || path.starts_with("/v1/completions")
        || path.starts_with("/v1/responses")
    {
        Some("codex")
    } else {
        None
    }
}

/// The tool named by a leading `/{tool}/` path segment, if it names one.
fn tool_prefix_of(path: &str) -> Option<&'static str> {
    let (first, _) = path.trim_start_matches('/').split_once('/')?;
    TOOLS.iter().find(|t| **t == first).copied()
}

/// The path without its `/{tool}` addressing prefix — what the gateway (or a
/// vendor upstream) is actually asked for.
///
/// The prefix is how the request reached the right buy switch; past that point
/// it is noise, and forwarding it would turn `/openclaw/v1/chat/completions`
/// into a 404 at the other end.
pub fn strip_tool_prefix(path: &str) -> &str {
    match tool_prefix_of(path) {
        // `tool_prefix_of` matched the *first* segment and required a `/` after
        // it, so the prefix is exactly an optional leading slash plus the name,
        // and what remains always starts with `/`.
        Some(tool) => &path.strip_prefix('/').unwrap_or(path)[tool.len()..],
        None => path,
    }
}

// ── Claude Code env keys ───────────────────────────────────────────────────
const ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const ANTHROPIC_AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
/// Claude Code also honours ANTHROPIC_API_KEY; clear it so a stale key can't
/// shadow the token we set (cc-switch resolves the same ambiguity).
const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";

// ── Gemini CLI env keys ────────────────────────────────────────────────────
const GEMINI_BASE_URL: &str = "GOOGLE_GEMINI_BASE_URL";
const GEMINI_API_KEY: &str = "GEMINI_API_KEY";

// ── Codex keys ─────────────────────────────────────────────────────────────
const CODEX_API_KEY: &str = "OPENAI_API_KEY";
/// The `[model_providers.<id>]` entry asale owns. Named distinctly so a restore
/// can identify exactly what we added without guessing.
const CODEX_PROVIDER_ID: &str = "asale";
/// Codex ≥ 0.146 refuses to load a config that still says `wire_api = "chat"`
/// ("`wire_api = \"chat\"` is no longer supported"), and a config it refuses to
/// load takes the whole switch with it — the tool silently keeps using its
/// pre-switch settings. The gateway serves `/v1/responses` for exactly this.
const CODEX_WIRE_API: &str = "responses";
/// Which model Codex starts a conversation with.
const CODEX_MODEL: &str = "model";
/// Path to the model list Codex's picker offers (see `codex_catalog`).
const CODEX_CATALOG: &str = "model_catalog_json";

// ── OpenClaw keys ──────────────────────────────────────────────────────────
//
// Shapes taken from a real `~/.openclaw/openclaw.json`, not from the docs: a
// working provider entry there carries `baseUrl` / `api` / `models[]`, and the
// agent picks one with `agents.defaults.model.primary = "<provider>/<model>"`.
/// The `models.providers.<id>` entry asale owns, named so a restore can find
/// exactly what we added.
const OPENCLAW_PROVIDER_ID: &str = "asale";
/// `merge` keeps OpenClaw's built-in providers alongside ours; without it a
/// custom block can replace the whole set.
const OPENCLAW_MODELS_MODE: &str = "merge";
/// The wire format we ask OpenClaw to speak. `openai-completions` is the one
/// this machine's own provider entry is already using, so it is the shape
/// proven to work rather than the one only documented.
const OPENCLAW_API: &str = "openai-completions";

// ── Hermes keys ────────────────────────────────────────────────────────────
//
// Shapes read from the repo's own `cli-config.yaml.example`, not the docs —
// which describe `auxiliary.compression.base_url`, a different (and wrong)
// section for this. Everything the primary model client needs lives under one
// top-level `model:` block.
/// The block the four keys below live in.
const HERMES_SECTION: &str = "model";
/// `custom` is Hermes' name for "any other OpenAI-compatible endpoint, see
/// base_url" — which is exactly what the local proxy is.
const HERMES_PROVIDER: &str = "provider";
const HERMES_PROVIDER_CUSTOM: &str = "custom";
const HERMES_BASE_URL: &str = "base_url";
const HERMES_API_KEY: &str = "api_key";
/// The model Hermes starts on. `default` and `model` are both accepted names;
/// we write the documented one.
const HERMES_MODEL: &str = "default";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// A tool's config directory (`~/.claude`, `~/.codex`, `~/.gemini`).
pub fn tool_dir(tool: &str) -> PathBuf {
    match tool {
        "claude" => home().join(".claude"),
        "codex" => home().join(".codex"),
        "gemini" => home().join(".gemini"),
        "openclaw" => home().join(".openclaw"),
        // Hermes is the one tool that does not live under a dot-directory in
        // `$HOME`. Its installer writes `HERMES_HOME` as a user environment
        // variable — `%LOCALAPPDATA%\hermes` on Windows — and the agent reads
        // its config from there; `~/.hermes` is only the fallback when that
        // variable is unset. Assuming the fallback pointed asale at a directory
        // that does not exist on a normal Windows install, so the switch wrote
        // a config Hermes would never read.
        "hermes" => std::env::var("HERMES_HOME")
            .ok()
            .map(|h| PathBuf::from(h.trim()))
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| home().join(".hermes")),
        _ => home().join(".asale-unknown"),
    }
}

/// Every file `apply` may rewrite, in write order.
pub fn config_paths(tool: &str) -> Vec<PathBuf> {
    let d = tool_dir(tool);
    match tool {
        "claude" => vec![d.join("settings.json")],
        "codex" => vec![d.join("config.toml"), d.join("auth.json")],
        "gemini" => vec![d.join(".env")],
        "openclaw" => vec![d.join("openclaw.json")],
        "hermes" => vec![d.join("config.yaml")],
        _ => vec![],
    }
}

/// The file shown to the user as "the config we switch".
pub fn primary_config_path(tool: &str) -> PathBuf {
    config_paths(tool).into_iter().next().unwrap_or_else(|| tool_dir(tool))
}

/// Is this tool present on the machine? True if its config dir/file exists or
/// its binary is on PATH. Detection only — never mutates anything.
pub fn installed(tool: &str) -> bool {
    if tool_dir(tool).is_dir() || config_paths(tool).iter().any(|p| p.exists()) {
        return true;
    }
    binary_on_path(match tool {
        "claude" => "claude",
        "codex" => "codex",
        "gemini" => "gemini",
        "openclaw" => "openclaw",
        "hermes" => "hermes",
        _ => return false,
    })
}

fn binary_on_path(name: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

/// The upstream base URL this tool is currently pointed at, if any — the UI uses
/// it to show whether the switch is actually in effect.
pub fn current_base_url(tool: &str) -> Option<String> {
    match tool {
        "claude" => read_json(&primary_config_path(tool))
            .get("env")?
            .get(ANTHROPIC_BASE_URL)?
            .as_str()
            .map(String::from),
        "gemini" => dotenv_get(&read_raw(&primary_config_path(tool)).unwrap_or_default(), GEMINI_BASE_URL),
        "codex" => {
            let raw = read_raw(&primary_config_path(tool))?;
            let doc = raw.parse::<toml_edit::DocumentMut>().ok()?;
            // Only the *active* provider counts; a stale inactive block must not
            // make the UI claim the switch is in effect.
            let active = doc.get("model_provider")?.as_str()?.to_string();
            doc.get("model_providers")?
                .get(&active)?
                .get("base_url")?
                .as_str()
                .map(String::from)
        }
        "hermes" => yaml_block_get(&read_raw(&primary_config_path(tool))?, HERMES_SECTION, HERMES_BASE_URL),
        "openclaw" => {
            let v = read_json(&primary_config_path(tool));
            // Same rule as Codex: only the provider the agent actually starts
            // on counts, so a leftover inactive block cannot make the UI claim
            // the switch is in effect. `primary` is "<provider>/<model>".
            let primary = v.get("agents")?.get("defaults")?.get("model")?.get("primary")?.as_str()?;
            let active = primary.split_once('/').map(|(p, _)| p).unwrap_or(primary);
            v.get("models")?
                .get("providers")?
                .get(active)?
                .get("baseUrl")?
                .as_str()
                .map(String::from)
        }
        _ => None,
    }
}

/// The exact base URL `tool`'s config should hold while it is buying.
///
/// Each tool addresses the proxy differently: Claude Code and Gemini CLI take
/// its origin, Codex the `/v1` root under it, and anything added since is
/// addressed under its own `/{tool}` prefix — which is what lets the proxy tell
/// tools apart when their dialects cannot (see [`for_request_path`]).
pub fn proxy_base_for(tool: &str) -> String {
    let base = proxy_base();
    match tool {
        "codex" => format!("{base}/v1"),
        "openclaw" | "hermes" => format!("{base}/{tool}/v1"),
        _ => base,
    }
}

/// The local proxy's origin — the base URL a bought-through tool is pointed at.
///
/// Read from the same environment `ClientConfig` does, so code holding only the
/// store can ask what a tool's config should say without the whole config being
/// threaded down to it.
pub fn proxy_base() -> String {
    format!("http://127.0.0.1:{}", asale_client_core::config::ClientConfig::default().proxy_port)
}

/// Is this tool's live config actually pointed at our own local proxy? Codex
/// addresses the proxy's `/v1` root, the others its origin.
pub fn points_at_proxy(tool: &str) -> bool {
    let base = proxy_base();
    let expected = proxy_base_for(tool);
    current_base_url(tool).is_some_and(|b| b == expected || b == base || b == format!("{base}/v1"))
}

/// Is this tool buying right now?
///
/// Either half is enough. The stored switch is what the user asked for; a
/// config whose base URL already names our proxy is what the tool is actually
/// doing, which still holds when the switch record was lost or was written by
/// an older build.
///
/// This is the sell side's exclusion rule: an account synced out of a tool's
/// own directory (`origin = "import"`) is not this device's to sell while that
/// tool is spending through the market. See `commands::accounts::list_accounts`,
/// `publisher::rebuild_pool` and `auth_store::resync`, which all ask it — the
/// list, the pool and the manifest have to agree, or an account would be hidden
/// from the user while still serving traffic.
///
/// The one function here that is not blocking: it needs the store, and every
/// caller is already async. The filesystem half goes to `spawn_blocking`.
pub async fn is_buying(store: &asale_client_core::store::LocalStore, tool: &str) -> bool {
    if !known(tool) {
        return false;
    }
    if store.buy_tool(tool).await.map(|r| r.enabled).unwrap_or(false) {
        return true;
    }
    let t = tool.to_string();
    tokio::task::spawn_blocking(move || points_at_proxy(&t)).await.unwrap_or(false)
}

/// Which of `providers` are buying, deduplicated.
///
/// The set form exists because [`is_buying`] may read a config file off disk,
/// and the pool rebuild runs on every status poll: asking once per rebuild
/// keeps that at one read per tool instead of one per account row.
pub async fn buying_set<'a>(
    store: &asale_client_core::store::LocalStore,
    providers: impl Iterator<Item = &'a str>,
) -> std::collections::HashSet<String> {
    let mut asked: Vec<&str> = Vec::new();
    let mut buying = std::collections::HashSet::new();
    for p in providers.filter(|p| known(p)) {
        if asked.contains(&p) {
            continue;
        }
        asked.push(p);
        if is_buying(store, p).await {
            buying.insert(p.to_string());
        }
    }
    buying
}

// ── Backup / restore ───────────────────────────────────────────────────────

/// The pre-switch content of one file. `raw: None` means the file did not exist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileBackup {
    pub path: String,
    pub raw: Option<String>,
}

/// A verbatim snapshot of every file `apply` rewrote, for an exact restore.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Backup {
    pub tool: String,
    pub files: Vec<FileBackup>,
}

impl Backup {
    /// True when at least one of the tool's files already existed.
    pub fn had_existing(&self) -> bool {
        self.files.iter().any(|f| f.raw.is_some())
    }
}

fn read_raw(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_json(path: &Path) -> Value {
    read_raw(path)
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| Value::Object(Map::new()))
}

/// Atomically write `body` to `path` (tmp + rename), creating the parent dir.
///
/// Also used by the refresh loop, which writes a rotated token back into the
/// CLI's own credential file — same requirement, same permissions.
pub(crate) fn write_atomic(path: &Path, body: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("config");
    let tmp = dir.join(format!("{name}.asale-tmp-{}", std::process::id()));
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    #[cfg(unix)]
    {
        // These files hold a bearer token — keep them owner-only.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Snapshot every file this tool's `apply` touches, before touching any of them.
fn snapshot(tool: &str) -> Backup {
    Backup {
        tool: tool.to_string(),
        files: config_paths(tool)
            .into_iter()
            .map(|p| FileBackup { path: p.to_string_lossy().to_string(), raw: read_raw(&p) })
            .collect(),
    }
}

/// Point `tool` at `base_url`, authenticating with `token`, preserving every
/// other setting the user had. Returns the pristine originals for a later
/// restore. Idempotent: re-applying only rewrites the keys asale owns.
///
/// `models` is the tool's buy selection (market model ids, empty = any). Only
/// Codex needs it: unlike Claude Code and Gemini CLI it does not take the model
/// from the caller's request — it picks from a catalog of its own, so the
/// selection has to be written into its config to have any effect at all.
pub fn apply(tool: &str, base_url: &str, token: &str, models: &[String]) -> Result<Backup> {
    anyhow::ensure!(known(tool), "unknown tool: {tool}");
    let backup = snapshot(tool);

    // Best-effort copy alongside the tool's own files, for the user.
    for f in &backup.files {
        if let Some(raw) = &f.raw {
            let backups = tool_dir(tool).join("backups");
            if std::fs::create_dir_all(&backups).is_ok() {
                let name = Path::new(&f.path).file_name().and_then(|n| n.to_str()).unwrap_or("config");
                let _ = std::fs::write(backups.join(format!("{name}.asale-pre-buy")), raw);
            }
        }
    }

    match tool {
        "claude" => apply_claude(base_url, token)?,
        "codex" => apply_codex(base_url, token, models)?,
        "gemini" => apply_gemini(base_url, token)?,
        "openclaw" => apply_openclaw(token, models)?,
        "hermes" => apply_hermes(token, models)?,
        _ => unreachable!("known() checked above"),
    }
    Ok(backup)
}

/// Restore the tool's config to its pre-switch state.
///   - file existed → written back byte-for-byte.
///   - file did not exist → strip the keys asale injected; if nothing
///     meaningful remains, remove the file so the machine looks untouched.
///     (Stripping rather than deleting outright matters when the user added
///     their own settings while the switch was on.)
pub fn restore(tool: &str, backup: &Backup) -> Result<()> {
    anyhow::ensure!(known(tool), "unknown tool: {tool}");
    for f in &backup.files {
        let path = PathBuf::from(&f.path);
        match &f.raw {
            Some(raw) => write_atomic(&path, raw)?,
            None => strip_ours(tool, &path)?,
        }
    }
    // The generated catalog lives outside the tool's own files, so restoring
    // those verbatim would otherwise leave it behind.
    if tool == "codex" {
        crate::codex_catalog::remove();
    }
    Ok(())
}

/// Restore when no backup was recorded (e.g. the switch was applied by an older
/// build, or the setting was lost): strip asale's keys from whatever is there.
pub fn strip_all(tool: &str) -> Result<()> {
    anyhow::ensure!(known(tool), "unknown tool: {tool}");
    for path in config_paths(tool) {
        strip_ours(tool, &path)?;
    }
    if tool == "codex" {
        crate::codex_catalog::remove();
    }
    Ok(())
}

// ── Claude Code ────────────────────────────────────────────────────────────

fn apply_claude(base_url: &str, token: &str) -> Result<()> {
    let path = primary_config_path("claude");
    let mut obj = read_json(&path).as_object().cloned().unwrap_or_default();
    let env = obj.entry("env".to_string()).or_insert_with(|| Value::Object(Map::new()));
    if !env.is_object() {
        *env = Value::Object(Map::new());
    }
    let env_obj = env.as_object_mut().expect("just ensured object");
    env_obj.insert(ANTHROPIC_BASE_URL.to_string(), Value::String(base_url.to_string()));
    env_obj.insert(ANTHROPIC_AUTH_TOKEN.to_string(), Value::String(token.to_string()));
    env_obj.remove(ANTHROPIC_API_KEY);
    write_atomic(&path, &serde_json::to_string_pretty(&Value::Object(obj))?)
}

// ── Codex ──────────────────────────────────────────────────────────────────

fn apply_codex(base_url: &str, token: &str, models: &[String]) -> Result<()> {
    let paths = config_paths("codex");
    let (config_path, auth_path) = (&paths[0], &paths[1]);

    // config.toml — format-preserving edit so the user's comments/layout survive.
    let raw = read_raw(config_path).unwrap_or_default();
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("{} is not valid TOML", config_path.display()))?;
    doc["model_provider"] = toml_edit::value(CODEX_PROVIDER_ID);
    let providers = doc
        .entry("model_providers")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    if let Some(tbl) = providers.as_table_mut() {
        // `[model_providers.asale]` renders as a dotted sub-table.
        tbl.set_implicit(true);
        let mut entry = toml_edit::Table::new();
        entry["name"] = toml_edit::value("asale");
        entry["base_url"] = toml_edit::value(format!("{}/v1", base_url.trim_end_matches('/')));
        // The proxy serves the Responses API, the only dialect this Codex
        // generation still speaks.
        entry["wire_api"] = toml_edit::value(CODEX_WIRE_API);
        tbl.insert(CODEX_PROVIDER_ID, toml_edit::Item::Table(entry));
    }

    // The buy selection: start on the first model, and offer the whole
    // selection in the picker. With no selection ("any model"), Codex keeps its
    // own model and catalog — there is nothing specific to point it at.
    match models.first() {
        Some(model) => match crate::codex_catalog::write(models)? {
            // Our catalog publishes market models under native slugs, so what
            // goes here is the carrier — the id Codex will actually send, which
            // the proxy translates back (see `codex_catalog`).
            Some(catalog) => {
                doc[CODEX_MODEL] = toml_edit::value(catalog.start_slug.as_str());
                doc[CODEX_CATALOG] = toml_edit::value(catalog.path.to_string_lossy().as_ref());
            }
            // No catalog could be generated: the picker keeps Codex's own
            // list, but `model` still points at what the user bought — with no
            // alias table to go through, the market id has to travel as itself.
            None => {
                drop_our_catalog(&mut doc, true);
                doc[CODEX_MODEL] = toml_edit::value(model.as_str());
            }
        },
        None => drop_our_catalog(&mut doc, true),
    }
    write_atomic(config_path, &doc.to_string())?;

    // auth.json — Codex reads the bearer for the active provider from here.
    let mut auth = read_json(auth_path).as_object().cloned().unwrap_or_default();
    auth.insert(CODEX_API_KEY.to_string(), Value::String(token.to_string()));
    write_atomic(auth_path, &serde_json::to_string_pretty(&Value::Object(auth))?)
}

/// Remove the catalog we generated, and with `also_model` the `model` key that
/// went with it.
///
/// `model_catalog_json` is only ever removed when it points at *our* file, so a
/// user who set up their own catalog keeps it. `model` carries no such marker,
/// which is why it is only dropped alongside a catalog we recognize as ours —
/// otherwise it is indistinguishable from a model the user chose themselves.
fn drop_our_catalog(doc: &mut toml_edit::DocumentMut, also_model: bool) {
    let ours = doc
        .get(CODEX_CATALOG)
        .and_then(|v| v.as_str())
        .is_some_and(crate::codex_catalog::is_ours);
    if ours {
        doc.remove(CODEX_CATALOG);
        if also_model {
            doc.remove(CODEX_MODEL);
        }
    }
    crate::codex_catalog::remove();
}

// ── OpenClaw ───────────────────────────────────────────────────────────────

/// Point OpenClaw at the proxy by adding a provider block it can select.
///
/// Unlike Codex, OpenClaw takes the model list from the config, so the bought
/// models travel under their own market ids — no carrier slugs, no alias table.
/// That is why there is no `codex_catalog` equivalent here.
///
/// `base_url` is not a parameter: OpenClaw is addressed under its own `/{tool}`
/// prefix, which [`proxy_base_for`] owns.
fn apply_openclaw(token: &str, models: &[String]) -> Result<()> {
    let path = primary_config_path("openclaw");
    let mut root = read_json(&path).as_object().cloned().unwrap_or_default();

    let mut provider = Map::new();
    provider.insert("baseUrl".into(), Value::String(proxy_base_for("openclaw")));
    provider.insert("apiKey".into(), Value::String(token.to_string()));
    provider.insert("api".into(), Value::String(OPENCLAW_API.to_string()));
    provider.insert(
        "models".into(),
        Value::Array(models.iter().map(|id| openclaw_model_entry(id)).collect()),
    );

    let models_obj = obj_entry(&mut root, "models");
    models_obj.insert("mode".into(), Value::String(OPENCLAW_MODELS_MODE.to_string()));
    let providers = obj_entry(models_obj, "providers");
    providers.insert(OPENCLAW_PROVIDER_ID.into(), Value::Object(provider));

    // Start on the first bought model. With no selection ("any model") there is
    // nothing specific to point at, so OpenClaw keeps whatever it was on — the
    // provider is still there to be picked from its own model list.
    if let Some(first) = models.first() {
        let defaults = obj_entry(obj_entry(&mut root, "agents"), "defaults");
        obj_entry(defaults, "model")
            .insert("primary".into(), Value::String(format!("{OPENCLAW_PROVIDER_ID}/{first}")));
    }
    write_atomic(&path, &serde_json::to_string_pretty(&Value::Object(root))?)
}

/// One `models[]` entry, in the shape OpenClaw's own provider blocks use.
///
/// The costs are zeroes because the market, not OpenClaw, does the billing —
/// these fields drive its local display only.
fn openclaw_model_entry(id: &str) -> Value {
    serde_json::json!({
        "id": id,
        "name": id,
        "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
    })
}

/// The object at `parent[key]`, created (or replaced, if it is not an object)
/// so a config with a surprising value there cannot fail the whole switch.
fn obj_entry<'a>(parent: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let slot = parent.entry(key.to_string()).or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    slot.as_object_mut().expect("just ensured object")
}

/// Drop the provider block asale added, and — if it still selects us — the
/// model the agent starts on. Everything else the user configured stays.
fn strip_openclaw(raw: &str) -> Option<String> {
    let mut root = serde_json::from_str::<Value>(raw).ok()?.as_object()?.clone();

    if let Some(Value::Object(models)) = root.get_mut("models") {
        if let Some(Value::Object(providers)) = models.get_mut("providers") {
            providers.remove(OPENCLAW_PROVIDER_ID);
            if providers.is_empty() {
                models.remove("providers");
            }
        }
        // `mode` is ours to remove only when nothing of ours is left to merge.
        if !models.contains_key("providers") {
            models.remove("mode");
        }
        if models.is_empty() {
            root.remove("models");
        }
    }

    // Only when it still names our provider — a model the user picked
    // themselves since is theirs to keep.
    let ours = root
        .get("agents")
        .and_then(|a| a.get("defaults"))
        .and_then(|d| d.get("model"))
        .and_then(|m| m.get("primary"))
        .and_then(Value::as_str)
        .is_some_and(|p| p.split_once('/').map(|(prov, _)| prov) == Some(OPENCLAW_PROVIDER_ID));
    if ours {
        // Prune the husk the removal leaves behind. `agents.defaults.model: {}`
        // is not something the user configured, and leaving it keeps the
        // document non-empty — which would make a file asale created itself
        // survive the restore that is supposed to remove it.
        if let Some(Value::Object(agents)) = root.get_mut("agents") {
            if let Some(Value::Object(defaults)) = agents.get_mut("defaults") {
                if let Some(Value::Object(model)) = defaults.get_mut("model") {
                    model.remove("primary");
                    if model.is_empty() {
                        defaults.remove("model");
                    }
                }
                if defaults.is_empty() {
                    agents.remove("defaults");
                }
            }
            if agents.is_empty() {
                root.remove("agents");
            }
        }
    }

    if root.is_empty() {
        return None;
    }
    serde_json::to_string_pretty(&Value::Object(root)).ok()
}

// ── Hermes ─────────────────────────────────────────────────────────────────

/// Point Hermes at the proxy: four scalar keys inside its `model:` block.
///
/// `base_url` is not a parameter — Hermes is addressed under its own `/{tool}`
/// prefix, which [`proxy_base_for`] owns.
fn apply_hermes(token: &str, models: &[String]) -> Result<()> {
    let path = primary_config_path("hermes");
    let raw = read_raw(&path).unwrap_or_default();
    let mut pairs = vec![
        (HERMES_PROVIDER, HERMES_PROVIDER_CUSTOM.to_string()),
        (HERMES_BASE_URL, proxy_base_for("hermes")),
        (HERMES_API_KEY, token.to_string()),
    ];
    // With no selection ("any model") Hermes keeps whatever it was on — there
    // is nothing specific to point it at.
    if let Some(first) = models.first() {
        pairs.push((HERMES_MODEL, first.clone()));
    }
    let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    write_atomic(&path, &yaml_block_set(&raw, HERMES_SECTION, &refs))
}

/// Remove the keys asale wrote — but only while `base_url` still names our
/// proxy. Past that the user has repointed Hermes themselves, and the values
/// are theirs rather than ours to delete.
fn strip_hermes(raw: &str) -> Option<String> {
    let ours =
        yaml_block_get(raw, HERMES_SECTION, HERMES_BASE_URL).is_some_and(|b| b == proxy_base_for("hermes"));
    if !ours {
        return Some(raw.to_string());
    }
    let out = yaml_block_remove(
        raw,
        HERMES_SECTION,
        &[HERMES_PROVIDER, HERMES_BASE_URL, HERMES_API_KEY, HERMES_MODEL],
    );
    // A bare `model:` with nothing under it is the husk our own keys left
    // behind, and it keeps the document non-empty — which would leave a file
    // asale created itself sitting on disk after the switch is turned off.
    let out = yaml_drop_empty_block(&out, HERMES_SECTION);
    if out.trim().is_empty() {
        return None;
    }
    Some(out)
}

// ── Minimal YAML block editing (comment- and order-preserving) ─────────────
//
// Hermes' config is a 1600-line commented template, and Rust has no
// format-preserving YAML editor the way `toml_edit` is for TOML — a serde
// round-trip would hand the file back stripped of every comment the user
// configures by. So the four keys asale owns are edited in place, line by
// line, exactly as the Gemini dotenv helpers below do for their format.
//
// Scope matters: `base_url` also appears under `auxiliary.compression`, so an
// edit not confined to one top-level block would repoint a section nobody
// asked about. Everything here works on one named block only.

/// A top-level `section:` block: its header line index, and the `[start, end)`
/// range of its body. `None` when the section is not there.
fn yaml_block(raw: &str, section: &str) -> Option<(usize, usize, usize)> {
    let lines: Vec<&str> = raw.lines().collect();
    let header = lines
        .iter()
        .position(|l| l.strip_prefix(section).is_some_and(|r| r.starts_with(':')))?;
    // The body ends at the next line starting a new top-level key: column 0,
    // not a comment, not blank.
    let end = lines[header + 1..]
        .iter()
        .position(|l| matches!(l.chars().next(), Some(c) if !c.is_whitespace() && c != '#'))
        .map(|i| header + 1 + i)
        .unwrap_or(lines.len());
    Some((header, header + 1, end))
}

/// A key's line index and indent inside a block body.
fn yaml_key_line(lines: &[&str], body: std::ops::Range<usize>, key: &str) -> Option<(usize, String)> {
    lines[body.clone()].iter().enumerate().find_map(|(i, l)| {
        let indent: String = l.chars().take_while(|c| c.is_whitespace()).collect();
        if indent.is_empty() {
            return None; // column 0 is not inside the block
        }
        // `key:` exactly — never a comment, and never a longer name that merely
        // starts with these characters.
        l[indent.len()..]
            .strip_prefix(key)
            .filter(|r| r.starts_with(':'))
            .map(|_| (body.start + i, indent))
    })
}

/// Read one scalar key out of a block.
fn yaml_block_get(raw: &str, section: &str, key: &str) -> Option<String> {
    let (_, body_start, body_end) = yaml_block(raw, section)?;
    let lines: Vec<&str> = raw.lines().collect();
    let (idx, indent) = yaml_key_line(&lines, body_start..body_end, key)?;
    let value = lines[idx][indent.len() + key.len() + 1..].trim();
    // An inline `#` ends the value only when it is not inside quotes.
    let value = match value.strip_prefix('"').and_then(|v| v.rfind('"').map(|e| v[..e].to_string())) {
        Some(quoted) => yaml_unescape(&quoted),
        None => value.split('#').next().unwrap_or("").trim().to_string(),
    };
    (!value.is_empty()).then_some(value)
}

/// Upsert scalar keys inside `section`, leaving every other line untouched.
/// The section (and the file) is created when missing.
fn yaml_block_set(raw: &str, section: &str, pairs: &[(&str, &str)]) -> String {
    let mut lines: Vec<String> = raw.lines().map(String::from).collect();
    if yaml_block(raw, section).is_none() {
        if !lines.is_empty() && !lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(format!("{section}:"));
    }
    for (key, value) in pairs {
        let joined = lines.join("\n");
        let (_, body_start, body_end) = yaml_block(&joined, section).expect("just ensured above");
        let view: Vec<&str> = lines.iter().map(String::as_str).collect();
        match yaml_key_line(&view, body_start..body_end, key) {
            Some((idx, indent)) => lines[idx] = format!("{indent}{key}: {}", yaml_scalar(value)),
            None => {
                // Insert after the block's last real content line, so the key
                // lands inside the block rather than after its trailing
                // comments — and inherits that line's indent.
                let last = (body_start..body_end).rev().find(|i| {
                    let l = &lines[*i];
                    !l.trim().is_empty() && !l.trim_start().starts_with('#') && l.starts_with(char::is_whitespace)
                });
                let (at, indent) = match last {
                    Some(i) => (i + 1, lines[i].chars().take_while(|c| c.is_whitespace()).collect::<String>()),
                    None => (body_start, "  ".to_string()),
                };
                lines.insert(at, format!("{indent}{key}: {}", yaml_scalar(value)));
            }
        }
    }
    let mut body = lines.join("\n");
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body
}

/// Delete keys from a block, leaving everything else — comments included — as
/// it was.
fn yaml_block_remove(raw: &str, section: &str, keys: &[&str]) -> String {
    let mut lines: Vec<String> = raw.lines().map(String::from).collect();
    for key in keys {
        let joined = lines.join("\n");
        let Some((_, body_start, body_end)) = yaml_block(&joined, section) else { break };
        let view: Vec<&str> = lines.iter().map(String::as_str).collect();
        if let Some((idx, _)) = yaml_key_line(&view, body_start..body_end, key) {
            lines.remove(idx);
        }
    }
    let mut body = lines.join("\n");
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body
}

/// Remove a `section:` header whose body has nothing left in it.
///
/// Only when the body is *entirely* blank: a comment under the header is
/// something the user wrote, and a block that still documents itself is not an
/// empty one, however few settings it currently holds.
fn yaml_drop_empty_block(raw: &str, section: &str) -> String {
    let Some((header, body_start, body_end)) = yaml_block(raw, section) else {
        return raw.to_string();
    };
    let lines: Vec<&str> = raw.lines().collect();
    if lines[body_start..body_end].iter().any(|l| !l.trim().is_empty()) {
        return raw.to_string();
    }
    let kept: Vec<&str> =
        lines.iter().enumerate().filter(|(i, _)| *i < header || *i >= body_end).map(|(_, l)| *l).collect();
    let mut body = kept.join("\n");
    if !body.trim().is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body
}

/// A double-quoted YAML scalar. Quoted unconditionally so a value that would
/// otherwise read as another type (a bare `1.0`, `yes`, `null`) stays the
/// string it is.
fn yaml_scalar(v: &str) -> String {
    let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Inverse of [`yaml_scalar`] for the two escapes it produces.
///
/// A reader that cannot undo its own writer is a trap waiting for the first
/// value with a quote in it. Other escapes YAML defines (`\n`, `\t`, …) are
/// passed through as written rather than decoded: nothing here emits them, and
/// guessing at a sequence we did not write would corrupt a value the user set.
fn yaml_unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some(next @ ('"' | '\\'))) => {
                out.push(next);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

// ── Gemini CLI ─────────────────────────────────────────────────────────────

fn apply_gemini(base_url: &str, token: &str) -> Result<()> {
    let path = primary_config_path("gemini");
    let raw = read_raw(&path).unwrap_or_default();
    let body = dotenv_set(&raw, &[(GEMINI_BASE_URL, base_url), (GEMINI_API_KEY, token)]);
    write_atomic(&path, &body)
}

// ── Stripping asale's keys back out ────────────────────────────────────────

fn strip_ours(tool: &str, path: &Path) -> Result<()> {
    let Some(raw) = read_raw(path) else { return Ok(()) };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let stripped = match (tool, name) {
        ("claude", _) => strip_json_env(&raw, &[ANTHROPIC_BASE_URL, ANTHROPIC_AUTH_TOKEN]),
        ("codex", "config.toml") => strip_codex_config(&raw),
        ("codex", "auth.json") => strip_json_keys(&raw, &[CODEX_API_KEY]),
        ("openclaw", _) => strip_openclaw(&raw),
        ("hermes", _) => strip_hermes(&raw),
        ("gemini", _) => strip_dotenv(&raw, &[GEMINI_BASE_URL, GEMINI_API_KEY]),
        _ => Some(raw),
    };
    match stripped {
        Some(body) => write_atomic(path, &body),
        // Nothing of the user's left — remove the file we created.
        None => {
            let _ = std::fs::remove_file(path);
            Ok(())
        }
    }
}

/// Remove keys from the top-level `env` object; `None` when the whole document
/// becomes empty.
fn strip_json_env(raw: &str, keys: &[&str]) -> Option<String> {
    let mut obj = serde_json::from_str::<Value>(raw).ok()?.as_object()?.clone();
    if let Some(Value::Object(env)) = obj.get_mut("env") {
        for k in keys {
            env.remove(*k);
        }
        if env.is_empty() {
            obj.remove("env");
        }
    }
    if obj.is_empty() {
        return None;
    }
    serde_json::to_string_pretty(&Value::Object(obj)).ok()
}

/// Remove top-level keys; `None` when the document becomes empty.
fn strip_json_keys(raw: &str, keys: &[&str]) -> Option<String> {
    let mut obj = serde_json::from_str::<Value>(raw).ok()?.as_object()?.clone();
    for k in keys {
        obj.remove(*k);
    }
    if obj.is_empty() {
        return None;
    }
    serde_json::to_string_pretty(&Value::Object(obj)).ok()
}

/// Drop `[model_providers.asale]`, our model selection, and — if it still
/// selects us — `model_provider`.
fn strip_codex_config(raw: &str) -> Option<String> {
    let mut doc = raw.parse::<toml_edit::DocumentMut>().ok()?;
    if doc.get("model_provider").and_then(|v| v.as_str()) == Some(CODEX_PROVIDER_ID) {
        doc.remove("model_provider");
    }
    drop_our_catalog(&mut doc, true);
    let drop_table = match doc.get_mut("model_providers").and_then(|p| p.as_table_mut()) {
        Some(tbl) => {
            tbl.remove(CODEX_PROVIDER_ID);
            tbl.is_empty()
        }
        None => false,
    };
    if drop_table {
        doc.remove("model_providers");
    }
    let out = doc.to_string();
    if out.trim().is_empty() {
        return None;
    }
    Some(out)
}

/// Remove `KEY=` lines; `None` when no non-blank line remains.
fn strip_dotenv(raw: &str, keys: &[&str]) -> Option<String> {
    let kept: Vec<&str> = raw
        .lines()
        .filter(|line| {
            let t = line.trim();
            match t.split_once('=') {
                Some((k, _)) if !t.starts_with('#') => !keys.contains(&k.trim()),
                _ => true,
            }
        })
        .collect();
    if kept.iter().all(|l| l.trim().is_empty()) {
        return None;
    }
    let mut body = kept.join("\n");
    if !body.ends_with('\n') {
        body.push('\n');
    }
    Some(body)
}

// ── Minimal dotenv helpers (order- and comment-preserving) ─────────────────

fn dotenv_get(raw: &str, key: &str) -> Option<String> {
    raw.lines().find_map(|line| {
        let t = line.trim();
        if t.starts_with('#') {
            return None;
        }
        let (k, v) = t.split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
    })
}

/// Upsert `pairs` into a dotenv document: existing keys are rewritten in place,
/// new ones appended, and every other line (including comments) is untouched.
fn dotenv_set(raw: &str, pairs: &[(&str, &str)]) -> String {
    let mut lines: Vec<String> = raw.lines().map(String::from).collect();
    for (key, value) in pairs {
        let replacement = format!("{key}={value}");
        let existing = lines.iter().position(|line| {
            let t = line.trim();
            !t.starts_with('#') && t.split_once('=').is_some_and(|(k, _)| k.trim() == *key)
        });
        match existing {
            Some(i) => lines[i] = replacement,
            None => lines.push(replacement),
        }
    }
    let mut body = lines.join("\n");
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_catalog::HOME_LOCK;

    fn with_temp_home<T>(f: impl FnOnce() -> T) -> T {
        let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("HOME").ok();
        let tmp = std::env::temp_dir().join(format!("asale-tool-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);
        // Never shell out to a real codex during tests: whether one is
        // installed must not change what these assert.
        std::env::set_var("ASALE_CODEX_BIN", tmp.join("codex-stub"));
        // `$HOME` does not contain Hermes: it resolves its own directory from
        // `HERMES_HOME`, which its installer sets to `%LOCALAPPDATA%\hermes` on
        // Windows. Left alone, every test here would read and rewrite the
        // machine's actual Hermes config.
        let prev_hermes = std::env::var("HERMES_HOME").ok();
        std::env::set_var("HERMES_HOME", tmp.join(".hermes"));
        let out = f();
        match prev {
            Some(p) => std::env::set_var("HOME", p),
            None => std::env::remove_var("HOME"),
        }
        match prev_hermes {
            Some(p) => std::env::set_var("HERMES_HOME", p),
            None => std::env::remove_var("HERMES_HOME"),
        }
        std::env::remove_var("ASALE_CODEX_BIN");
        let _ = std::fs::remove_dir_all(&tmp);
        out
    }

    /// Install a fake `codex debug models` that prints `slugs` as its catalog,
    /// most-preferred carrier first.
    fn codex_stub(slugs: &[&str]) {
        let entries: Vec<String> = slugs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                format!(r#"{{"slug":"{s}","visibility":"list","priority":{i},"base_instructions":"native"}}"#)
            })
            .collect();
        let path = home().join("codex-stub");
        std::fs::write(
            &path,
            format!("#!/bin/sh\ncat <<'JSON'\n{{\"models\":[{}]}}\nJSON\n", entries.join(",")),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn models(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn claude_apply_preserves_other_keys_and_restore_is_verbatim() {
        with_temp_home(|| {
            let path = primary_config_path("claude");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let original = "{\n  \"env\": { \"FOO\": \"bar\", \"ANTHROPIC_API_KEY\": \"sk-old\" },\n  \"permissions\": { \"allow\": [\"Bash\"] }\n}";
            std::fs::write(&path, original).unwrap();

            let backup = apply("claude", "http://127.0.0.1:9787", "sk-asale-xyz", &[]).unwrap();
            assert!(backup.had_existing());

            let obj = read_json(&path);
            let env = obj.get("env").unwrap();
            assert_eq!(env.get(ANTHROPIC_BASE_URL).unwrap(), "http://127.0.0.1:9787");
            assert_eq!(env.get(ANTHROPIC_AUTH_TOKEN).unwrap(), "sk-asale-xyz");
            assert_eq!(env.get("FOO").unwrap(), "bar", "unrelated env preserved");
            assert!(env.get(ANTHROPIC_API_KEY).is_none(), "stale api key cleared");
            assert!(obj.get("permissions").is_some(), "unrelated keys preserved");
            assert_eq!(current_base_url("claude").as_deref(), Some("http://127.0.0.1:9787"));

            restore("claude", &backup).unwrap();
            assert_eq!(read_raw(&path).unwrap(), original, "restore is byte-exact");
        });
    }

    #[test]
    fn claude_apply_creates_file_and_restore_removes_it() {
        with_temp_home(|| {
            let path = primary_config_path("claude");
            assert!(read_raw(&path).is_none());
            let backup = apply("claude", "http://127.0.0.1:9787", "sk-1", &[]).unwrap();
            assert!(!backup.had_existing());
            restore("claude", &backup).unwrap();
            assert!(read_raw(&path).is_none(), "file removed since it never existed");
        });
    }

    #[test]
    fn codex_switches_active_provider_and_restores() {
        with_temp_home(|| {
            codex_stub(&["gpt-5.5", "gpt-5.2", "gpt-5.4"]);
            let paths = config_paths("codex");
            std::fs::create_dir_all(paths[0].parent().unwrap()).unwrap();
            let original = "model = \"gpt-5-codex\"\nmodel_provider = \"openai\"\n\n[model_providers.openai]\nname = \"OpenAI\"\nbase_url = \"https://api.openai.com/v1\"\n";
            std::fs::write(&paths[0], original).unwrap();

            let backup = apply(
                "codex",
                "http://127.0.0.1:9787",
                "sk-asale-codex",
                &models(&["claude-fable-5", "claude-opus-5"]),
            )
            .unwrap();
            assert_eq!(current_base_url("codex").as_deref(), Some("http://127.0.0.1:9787/v1"));
            let cfg = read_raw(&paths[0]).unwrap();
            assert!(cfg.contains("[model_providers.openai]"), "user's provider preserved");
            assert_eq!(read_json(&paths[1]).get(CODEX_API_KEY).unwrap(), "sk-asale-codex");

            let doc = cfg.parse::<toml_edit::DocumentMut>().unwrap();
            assert_eq!(
                doc["model_providers"][CODEX_PROVIDER_ID]["wire_api"].as_str(),
                Some("responses"),
                "`chat` is rejected outright by Codex >= 0.146"
            );
            assert_eq!(
                doc[CODEX_MODEL].as_str(),
                Some("gpt-5.5"),
                "starts on the slug carrying the first bought model, not the market id"
            );
            let catalog = doc[CODEX_CATALOG].as_str().unwrap();
            assert!(crate::codex_catalog::is_ours(catalog), "points at the catalog we generated");

            // Both bought models are offered, each wearing a native slug so the
            // desktop app's allowlist lets it through; the spare native is kept
            // for the app's internal lookups but taken out of the picker.
            let listed = std::fs::read_to_string(catalog).unwrap();
            let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
            let slugs: Vec<(&str, &str, &str)> = listed["models"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| {
                    (
                        m["slug"].as_str().unwrap(),
                        m["visibility"].as_str().unwrap(),
                        m["display_name"].as_str().unwrap_or_default(),
                    )
                })
                .collect();
            assert_eq!(
                slugs,
                [
                    ("gpt-5.5", "list", "claude-fable-5"),
                    ("gpt-5.2", "list", "claude-opus-5"),
                    ("gpt-5.4", "hide", ""),
                ]
            );
            assert_eq!(crate::codex_catalog::alias_for("gpt-5.5").as_deref(), Some("claude-fable-5"));
            assert_eq!(crate::codex_catalog::alias_for("gpt-5.4"), None, "an unused carrier stands for nothing");

            restore("codex", &backup).unwrap();
            assert_eq!(read_raw(&paths[0]).unwrap(), original, "restore is byte-exact");
            assert!(read_raw(&paths[1]).is_none(), "auth.json we created is removed");
            assert!(!crate::codex_catalog::path().exists(), "the generated catalog goes with it");
            assert!(!crate::codex_catalog::aliases_path().exists(), "and the alias table");
        });
    }

    #[test]
    fn codex_without_a_selection_leaves_the_model_and_catalog_alone() {
        with_temp_home(|| {
            codex_stub(&["gpt-5.2"]);
            let paths = config_paths("codex");
            std::fs::create_dir_all(paths[0].parent().unwrap()).unwrap();
            std::fs::write(&paths[0], "model = \"gpt-5-codex\"\n").unwrap();

            apply("codex", "http://127.0.0.1:9787", "sk-asale-codex", &[]).unwrap();

            let doc = read_raw(&paths[0]).unwrap().parse::<toml_edit::DocumentMut>().unwrap();
            assert_eq!(doc[CODEX_MODEL].as_str(), Some("gpt-5-codex"), "'any model' keeps the user's own");
            assert!(doc.get(CODEX_CATALOG).is_none(), "and its own picker");
            assert!(!crate::codex_catalog::path().exists());
        });
    }

    #[test]
    fn codex_strip_leaves_users_own_provider_and_catalog() {
        with_temp_home(|| {
            let ours = crate::codex_catalog::path();
            let raw = format!(
                "model = \"claude-fable-5\"\nmodel_catalog_json = \"{}\"\nmodel_provider = \"asale\"\n\n[model_providers.asale]\nbase_url = \"http://x/v1\"\n\n[model_providers.openai]\nbase_url = \"https://api.openai.com/v1\"\n",
                ours.display()
            );
            let out = strip_codex_config(&raw).unwrap();
            assert!(!out.contains("model_providers.asale"), "asale block dropped");
            assert!(!out.contains("model_provider ="), "selection of asale dropped");
            assert!(!out.contains("model_catalog_json"), "our catalog dropped");
            assert!(!out.contains("model ="), "the model that came with our catalog dropped");
            assert!(out.contains("[model_providers.openai]"), "user's provider kept");

            // A catalog the user set up themselves, and the model that goes
            // with it, must survive untouched.
            let theirs = "model = \"gpt-5.2\"\nmodel_catalog_json = \"/home/me/models.json\"\nmodel_provider = \"asale\"\n";
            let out = strip_codex_config(theirs).unwrap();
            assert!(out.contains("model_catalog_json = \"/home/me/models.json\""));
            assert!(out.contains("model = \"gpt-5.2\""));
        });
    }

    #[test]
    fn request_paths_resolve_the_tool_by_prefix_then_by_dialect() {
        // Prefix wins: this is the case the dialect cannot decide.
        assert_eq!(for_request_path("/openclaw/v1/chat/completions"), Some("openclaw"));
        assert_eq!(for_request_path("/openclaw/v1/messages"), Some("openclaw"));
        // Hermes speaks the same OpenAI wire as OpenClaw and Codex — three
        // tools, one dialect, told apart only by this prefix.
        assert_eq!(for_request_path("/hermes/v1/chat/completions"), Some("hermes"));
        assert_eq!(strip_tool_prefix("/hermes/v1/chat/completions"), "/v1/chat/completions");
        // Unprefixed keeps the old dialect inference, so existing configs work.
        assert_eq!(for_request_path("/v1/messages"), Some("claude"));
        assert_eq!(for_request_path("/v1/chat/completions"), Some("codex"));
        assert_eq!(for_request_path("/v1beta/models/x"), Some("gemini"));
        // A prefix we do not know names no tool — the caller refuses it.
        assert_eq!(for_request_path("/not-a-tool/v1/messages"), None);
        assert_eq!(for_request_path("/healthz"), None);

        assert_eq!(strip_tool_prefix("/openclaw/v1/chat/completions"), "/v1/chat/completions");
        assert_eq!(strip_tool_prefix("/openclaw/v1/models?x=1"), "/v1/models?x=1");
        assert_eq!(strip_tool_prefix("/v1/messages"), "/v1/messages", "nothing to strip");
        assert_eq!(strip_tool_prefix("/not-a-tool/v1/messages"), "/not-a-tool/v1/messages");
    }

    #[test]
    fn openclaw_adds_a_provider_and_restore_is_verbatim() {
        with_temp_home(|| {
            let path = primary_config_path("openclaw");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            // Shaped like the real file: a provider of the user's own, and an
            // agent already pointed at it.
            let original = r#"{
  "models": {"mode": "merge", "providers": {"qwen": {"baseUrl": "https://dashscope.aliyuncs.com/compatible-mode/v1", "api": "openai-completions"}}},
  "agents": {"defaults": {"workspace": "C:/ws", "model": {"primary": "qwen/qwen3.5-plus"}}},
  "tools": {"profile": "coding"}
}"#;
            std::fs::write(&path, original).unwrap();

            let backup = apply("openclaw", "http://127.0.0.1:9787", "sk-asale-oc", &models(&["claude-fable-5", "claude-opus-5"])).unwrap();

            let v = read_json(&path);
            let ours = &v["models"]["providers"][OPENCLAW_PROVIDER_ID];
            assert_eq!(ours["baseUrl"], "http://127.0.0.1:9787/openclaw/v1", "addressed under its own prefix");
            assert_eq!(ours["apiKey"], "sk-asale-oc");
            assert_eq!(ours["api"], "openai-completions");
            let ids: Vec<&str> = ours["models"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
            assert_eq!(ids, ["claude-fable-5", "claude-opus-5"], "market ids travel as themselves — no carrier slugs");
            assert_eq!(v["agents"]["defaults"]["model"]["primary"], "asale/claude-fable-5", "starts on the first bought model");
            assert!(v["models"]["providers"]["qwen"].is_object(), "the user's own provider survives");
            assert_eq!(v["agents"]["defaults"]["workspace"], "C:/ws", "unrelated settings survive");
            assert_eq!(v["tools"]["profile"], "coding");
            assert_eq!(current_base_url("openclaw").as_deref(), Some("http://127.0.0.1:9787/openclaw/v1"));
            assert!(points_at_proxy("openclaw"));

            restore("openclaw", &backup).unwrap();
            assert_eq!(read_raw(&path).unwrap(), original, "restore is byte-exact");
        });
    }

    #[test]
    fn openclaw_strip_keeps_what_the_user_configured() {
        let raw = r#"{
  "models": {"mode": "merge", "providers": {"asale": {"baseUrl": "http://x/openclaw/v1"}, "qwen": {"baseUrl": "https://q"}}},
  "agents": {"defaults": {"model": {"primary": "asale/claude-fable-5"}}}
}"#;
        let out = strip_openclaw(raw).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["models"]["providers"].get("asale").is_none(), "our block goes");
        assert!(v["models"]["providers"]["qwen"].is_object(), "theirs stays");
        assert!(v["agents"]["defaults"]["model"].get("primary").is_none(), "the selection of us goes with it");

        // A model the user picked since is theirs, not ours to remove.
        let theirs = r#"{"models": {"providers": {"asale": {}}}, "agents": {"defaults": {"model": {"primary": "qwen/qwen3.5-plus"}}}}"#;
        let v: Value = serde_json::from_str(&strip_openclaw(theirs).unwrap()).unwrap();
        assert_eq!(v["agents"]["defaults"]["model"]["primary"], "qwen/qwen3.5-plus");
    }

    #[test]
    fn openclaw_apply_creates_a_usable_file_and_restore_removes_it() {
        with_temp_home(|| {
            let path = primary_config_path("openclaw");
            let backup = apply("openclaw", "http://127.0.0.1:9787", "sk-1", &models(&["claude-fable-5"])).unwrap();
            assert!(!backup.had_existing());
            assert!(read_json(&path)["models"]["providers"]["asale"].is_object());
            restore("openclaw", &backup).unwrap();
            assert!(read_raw(&path).is_none(), "file removed since it never existed");
        });
    }

    /// Shaped like the real `cli-config.yaml.example`: a commented `model:`
    /// block, and a *second* `base_url` under another section.
    const HERMES_YAML: &str = "\
# Hermes config
model:
  # Default model to use
  default: \"anthropic/claude-opus-4.6\"

  # Inference provider selection
  provider: \"auto\"
  base_url: \"https://openrouter.ai/api/v1\"

  # context_length: 131072

auxiliary:
  compression:
    model: \"glm-4.7\"
    base_url: https://api.z.ai/api/coding/paas/v4
";

    #[test]
    fn hermes_edits_only_its_own_block_and_keeps_every_comment() {
        with_temp_home(|| {
            let path = primary_config_path("hermes");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, HERMES_YAML).unwrap();

            let backup = apply("hermes", "http://127.0.0.1:9787", "sk-asale-h", &models(&["gpt-5.6-terra"])).unwrap();
            let out = read_raw(&path).unwrap();

            assert_eq!(yaml_block_get(&out, "model", "provider").as_deref(), Some("custom"));
            assert_eq!(yaml_block_get(&out, "model", "base_url").as_deref(), Some("http://127.0.0.1:9787/hermes/v1"));
            assert_eq!(yaml_block_get(&out, "model", "api_key").as_deref(), Some("sk-asale-h"));
            assert_eq!(yaml_block_get(&out, "model", "default").as_deref(), Some("gpt-5.6-terra"));
            assert!(points_at_proxy("hermes"));

            // The other section's `base_url` is a different setting entirely.
            assert!(
                out.contains("base_url: https://api.z.ai/api/coding/paas/v4"),
                "an edit that is not block-scoped repoints a section nobody asked about"
            );
            for comment in ["# Hermes config", "# Default model to use", "# context_length: 131072"] {
                assert!(out.contains(comment), "lost comment {comment:?}");
            }

            restore("hermes", &backup).unwrap();
            assert_eq!(read_raw(&path).unwrap(), HERMES_YAML, "restore is byte-exact");
        });
    }

    #[test]
    fn hermes_strip_only_touches_a_config_still_pointed_at_us() {
        with_temp_home(|| {
            let ours = yaml_block_set(
                HERMES_YAML,
                "model",
                &[("base_url", &proxy_base_for("hermes")), ("api_key", "sk-asale-h")],
            );
            let out = strip_hermes(&ours).unwrap();
            assert!(yaml_block_get(&out, "model", "base_url").is_none(), "our endpoint goes");
            assert!(yaml_block_get(&out, "model", "api_key").is_none(), "and the key with it");
            assert!(out.contains("# Default model to use"), "comments survive the strip too");
            assert!(out.contains("base_url: https://api.z.ai/api/coding/paas/v4"), "the other block is untouched");

            // Repointed by the user since: not ours to edit any more.
            assert_eq!(strip_hermes(HERMES_YAML).as_deref(), Some(HERMES_YAML));
        });
    }

    #[test]
    fn hermes_config_asale_created_is_removed_again() {
        with_temp_home(|| {
            let path = primary_config_path("hermes");
            assert!(read_raw(&path).is_none(), "nothing here to begin with");

            let backup = apply("hermes", "http://127.0.0.1:9787", "sk-1", &models(&["gpt-5.6-terra"])).unwrap();
            assert!(!backup.had_existing());
            assert!(points_at_proxy("hermes"), "a config was created and is in effect");

            restore("hermes", &backup).unwrap();
            assert!(read_raw(&path).is_none(), "the file asale created goes with the switch");
        });
    }

    #[test]
    fn an_empty_block_is_dropped_but_a_documented_one_is_kept() {
        assert_eq!(yaml_drop_empty_block("model:\n", "model"), "");
        assert_eq!(yaml_drop_empty_block("other: 1\nmodel:\n\n", "model"), "other: 1\n");
        // The user's own comment is content — the block stays.
        let documented = "model:\n  # set provider here\n";
        assert_eq!(yaml_drop_empty_block(documented, "model"), documented);
    }

    #[test]
    fn yaml_block_editing_handles_the_awkward_shapes() {
        // A key absent from the block is inserted into it, not appended to the
        // file — where it would belong to whatever section came last.
        let out = yaml_block_set(HERMES_YAML, "model", &[("api_key", "k")]);
        let model_block: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.starts_with("model:"))
            .skip(1)
            .take_while(|l| !l.starts_with("auxiliary:"))
            .collect();
        assert!(model_block.iter().any(|l| l.trim() == "api_key: \"k\""), "landed outside its block: {model_block:?}");

        // A name that merely starts with the key is not the key.
        let raw = "model:\n  base_url_fallback: \"x\"\n";
        assert_eq!(yaml_block_get(raw, "model", "base_url"), None);
        let out = yaml_block_set(raw, "model", &[("base_url", "y")]);
        assert!(out.contains("base_url_fallback: \"x\""), "clobbered a longer name");
        assert!(out.contains("base_url: \"y\""));

        // A missing section (and an empty file) is created.
        let out = yaml_block_set("other: 1\n", "model", &[("provider", "custom")]);
        assert_eq!(yaml_block_get(&out, "model", "provider").as_deref(), Some("custom"));
        assert!(out.starts_with("other: 1"), "the existing document is kept");
        assert_eq!(yaml_block_get(&yaml_block_set("", "model", &[("provider", "custom")]), "model", "provider").as_deref(), Some("custom"));

        // Commented-out keys are documentation, not settings.
        let raw = "model:\n  # provider: \"auto\"\n";
        assert_eq!(yaml_block_get(raw, "model", "provider"), None);
        let out = yaml_block_set(raw, "model", &[("provider", "custom")]);
        assert!(out.contains("# provider: \"auto\""), "the comment is still there");
        assert_eq!(yaml_block_get(&out, "model", "provider").as_deref(), Some("custom"));

        // Values with characters YAML would otherwise read as syntax.
        let out = yaml_block_set("", "model", &[("api_key", "a\"b\\c#d")]);
        assert_eq!(yaml_block_get(&out, "model", "api_key").as_deref(), Some("a\"b\\c#d"));
        // An unquoted value with a trailing comment reads as the value only.
        assert_eq!(yaml_block_get("model:\n  provider: auto # why\n", "model", "provider").as_deref(), Some("auto"));
    }

    #[test]
    fn gemini_env_upsert_preserves_comments_and_restores() {
        with_temp_home(|| {
            let path = primary_config_path("gemini");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let original = "# my gemini env\nGEMINI_API_KEY=user-key\nOTHER=1\n";
            std::fs::write(&path, original).unwrap();

            let backup = apply("gemini", "http://127.0.0.1:9787", "sk-asale-gem", &[]).unwrap();
            let body = read_raw(&path).unwrap();
            assert!(body.starts_with("# my gemini env"), "comments preserved");
            assert!(body.contains("OTHER=1"), "unrelated vars preserved");
            assert_eq!(dotenv_get(&body, GEMINI_API_KEY).as_deref(), Some("sk-asale-gem"), "key replaced in place");
            assert_eq!(current_base_url("gemini").as_deref(), Some("http://127.0.0.1:9787"));

            restore("gemini", &backup).unwrap();
            assert_eq!(read_raw(&path).unwrap(), original);
        });
    }

    #[test]
    fn strip_dotenv_keeps_the_users_lines() {
        assert_eq!(
            strip_dotenv("GOOGLE_GEMINI_BASE_URL=http://x\nOTHER=1\n", &[GEMINI_BASE_URL]).unwrap(),
            "OTHER=1\n"
        );
        assert!(strip_dotenv("GOOGLE_GEMINI_BASE_URL=http://x\n", &[GEMINI_BASE_URL]).is_none());
    }
}
