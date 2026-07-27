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
pub const TOOLS: &[&str] = &["claude", "codex", "gemini"];

/// Display name for a tool id.
pub fn label(tool: &str) -> &'static str {
    match tool {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "gemini" => "Gemini CLI",
        _ => "unknown",
    }
}

/// Reject unknown ids at the API boundary.
pub fn known(tool: &str) -> bool {
    TOOLS.contains(&tool)
}

/// Which locally installed CLI a proxied request came from, inferred from the
/// dialect its path speaks. `None` for paths no tool owns (e.g. `/healthz`).
///
/// Lives here, beside [`TOOLS`], because it is the same knowledge: adding a
/// fourth tool means adding it to both, and having them in one file is what
/// makes that obvious.
pub fn for_request_path(path: &str) -> Option<&'static str> {
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

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// A tool's config directory (`~/.claude`, `~/.codex`, `~/.gemini`).
pub fn tool_dir(tool: &str) -> PathBuf {
    match tool {
        "claude" => home().join(".claude"),
        "codex" => home().join(".codex"),
        "gemini" => home().join(".gemini"),
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
        _ => None,
    }
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
fn write_atomic(path: &Path, body: &str) -> Result<()> {
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
        let out = f();
        match prev {
            Some(p) => std::env::set_var("HOME", p),
            None => std::env::remove_var("HOME"),
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
