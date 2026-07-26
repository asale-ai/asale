//! The model catalog Codex shows in its picker.
//!
//! Codex does not ask a provider which models it serves — no `/v1/models` call
//! is ever made. Its picker is fed by a catalog compiled into the binary
//! (`codex debug models`), which is why a model bought on asale would otherwise
//! never appear in the ChatGPT app's model list no matter what the buy page is
//! set to. The one supported override is the `model_catalog_json` config key:
//! a path to a JSON document that *replaces* the built-in catalog wholesale.
//!
//! So while the codex buy switch is on we generate that document:
//!   - every native entry is kept but forced to `visibility: "hide"`, so the
//!     app's internal lookups (auto-review and friends) still resolve while the
//!     picker stops offering models the market cannot serve;
//!   - one visible entry per selected market model, cloned from a native entry
//!     so it inherits a schema this exact Codex build accepts.
//!
//! Cloning rather than hand-authoring matters: the entry schema is large
//! (~30 fields, including the harness prompt) and unversioned. If no codex
//! binary can be found to dump a template, we write no catalog at all and leave
//! the picker alone — `model` in config.toml still points at the bought model,
//! so the switch works, it just is not browsable.

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long `codex debug models` may take before we give up on it.
const DUMP_TIMEOUT: Duration = Duration::from_secs(20);

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// Where the generated catalog lives. Kept in asale's own directory, not in
/// `~/.codex`, so a restore never has to guess whether a file there was ours.
pub fn path() -> PathBuf {
    home().join(".asale").join("codex-models.json")
}

/// Generate the catalog for `models` (market model ids, in picker order).
///
/// `Ok(None)` means no catalog could be built — the caller must then leave
/// `model_catalog_json` unset rather than pointing Codex at a file we could not
/// validate.
pub fn write(models: &[String]) -> Result<Option<PathBuf>> {
    if models.is_empty() {
        return Ok(None);
    }
    let Some(native) = native_catalog() else {
        tracing::warn!("no codex binary found to dump a model catalog; the picker will not list asale models");
        return Ok(None);
    };
    let Some(template) = pick_template(&native) else {
        tracing::warn!("codex model catalog has no usable template entry");
        return Ok(None);
    };

    let mut entries: Vec<Value> = Vec::new();
    for m in native {
        // A native slug the market also sells would collide with our own entry.
        if models.iter().any(|s| s == slug_of(&m)) {
            continue;
        }
        entries.push(hidden(m));
    }
    for (i, m) in models.iter().enumerate() {
        entries.push(entry(&template, m, i));
    }

    let target = path();
    let body = serde_json::to_string_pretty(&json!({ "models": entries }))?;
    write_atomic(&target, &body)?;
    Ok(Some(target))
}

/// Drop the generated catalog. Called when the switch goes off, so nothing we
/// wrote outlives the switch.
pub fn remove() {
    let _ = std::fs::remove_file(path());
}

/// Does this config value point at the catalog we generate? Used by the
/// restore path to strip only our own `model_catalog_json`.
pub fn is_ours(value: &str) -> bool {
    Path::new(value) == path()
}

// ── native catalog ─────────────────────────────────────────────────────────

/// Codex binaries to ask for the built-in catalog, most specific first. The
/// ChatGPT app bundles its own; a standalone CLI may also be installed. Both
/// read the same `~/.codex/config.toml`, so one catalog serves whichever is
/// present.
fn codex_binaries() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // Escape hatch for an install we do not know about (and what the tests
    // point at, so they never depend on a codex being installed).
    if let Ok(explicit) = std::env::var("ASALE_CODEX_BIN") {
        return vec![PathBuf::from(explicit)];
    }
    let bundled = PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex");
    if bundled.is_file() {
        out.push(bundled);
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("codex");
            if candidate.is_file() {
                out.push(candidate);
                break;
            }
        }
    }
    out
}

/// `codex debug models` → the built-in model entries.
///
/// Run against a scratch `CODEX_HOME`: the user's own config is mid-rewrite
/// when this is called, and a config Codex refuses to parse (which is exactly
/// the state an older asale build leaves behind) would take the dump with it.
fn native_catalog() -> Option<Vec<Value>> {
    let scratch = std::env::temp_dir().join(format!("asale-codex-catalog-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let out_path = scratch.join("models.json");

    let mut found = None;
    for bin in codex_binaries() {
        if let Some(models) = dump_models(&bin, &scratch, &out_path) {
            found = Some(models);
            break;
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    found
}

fn dump_models(bin: &Path, scratch: &Path, out_path: &Path) -> Option<Vec<Value>> {
    // Straight to a file, never a pipe: the catalog carries the full harness
    // prompt for every model (hundreds of KiB), which would deadlock against an
    // undrained pipe buffer.
    let out_file = std::fs::File::create(out_path).ok()?;
    let mut child = Command::new(bin)
        .args(["debug", "models"])
        .env("CODEX_HOME", scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| tracing::warn!("running {}: {e}", bin.display()))
        .ok()?;

    let deadline = Instant::now() + DUMP_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                tracing::warn!("{} debug models timed out", bin.display());
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                tracing::warn!("waiting on {}: {e}", bin.display());
                return None;
            }
        }
    };
    if !status.success() {
        tracing::warn!("{} debug models exited with {status}", bin.display());
        return None;
    }
    let raw = std::fs::read_to_string(out_path).ok()?;
    parse_models(&raw)
}

/// Accept both the documented `{"models": [...]}` envelope and a bare array,
/// so a future dump format change degrades to "no catalog" only if it is
/// genuinely unrecognizable.
fn parse_models(raw: &str) -> Option<Vec<Value>> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let models = match v {
        Value::Object(mut o) => o.remove("models")?,
        arr @ Value::Array(_) => arr,
        _ => return None,
    };
    match models {
        Value::Array(a) if !a.is_empty() => Some(a),
        _ => None,
    }
}

// ── entry synthesis ────────────────────────────────────────────────────────

fn slug_of(entry: &Value) -> &str {
    entry.get("slug").and_then(Value::as_str).unwrap_or_default()
}

/// The native entry asale models are cloned from: the highest-priority one the
/// picker lists, skipping `use_responses_lite` models whose slimmed-down
/// request shape is tied to a specific OpenAI model generation.
fn pick_template(models: &[Value]) -> Option<Value> {
    let usable = |m: &&Value| {
        m.get("visibility").and_then(Value::as_str) == Some("list")
            && m.get("use_responses_lite").and_then(Value::as_bool) != Some(true)
    };
    let by_priority = |m: &&Value| m.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX);
    models
        .iter()
        .filter(usable)
        .min_by_key(by_priority)
        .or_else(|| models.iter().min_by_key(by_priority))
        .cloned()
}

/// A native entry, kept for internal lookups but out of the picker.
fn hidden(mut entry: Value) -> Value {
    if let Some(o) = entry.as_object_mut() {
        o.insert("visibility".into(), json!("hide"));
    }
    entry
}

/// One market model, wearing a native entry's schema.
fn entry(template: &Value, model: &str, order: usize) -> Value {
    let mut e = template.clone();
    let Some(o) = e.as_object_mut() else { return e };
    set(o, "slug", json!(model));
    set(o, "display_name", json!(model));
    set(o, "description", json!("Bought through the Asale market."));
    set(o, "visibility", json!("list"));
    set(o, "supported_in_api", json!(true));
    // Ahead of every hidden native entry, in the order the buy page lists them.
    set(o, "priority", json!(order as i64 + 1));
    // Upgrade banners and first-run nudges belong to the model we copied.
    set(o, "upgrade", Value::Null);
    set(o, "availability_nux", Value::Null);
    e
}

fn set(o: &mut Map<String, Value>, key: &str, value: Value) {
    o.insert(key.to_string(), value);
}

fn write_atomic(path: &Path, body: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!("codex-models.json.asale-tmp-{}", std::process::id()));
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native() -> Vec<Value> {
        vec![
            json!({"slug": "gpt-5.6-sol", "visibility": "list", "priority": 1,
                   "use_responses_lite": true, "base_instructions": "sol prompt"}),
            json!({"slug": "gpt-5.2", "visibility": "list", "priority": 29,
                   "use_responses_lite": false, "base_instructions": "5.2 prompt",
                   "upgrade": {"copy": "try sol"}}),
            json!({"slug": "codex-auto-review", "visibility": "hide", "priority": 43}),
        ]
    }

    #[test]
    fn template_skips_responses_lite_models() {
        // sol has the better priority but a request shape tied to its own
        // generation; 5.2 is the highest-priority entry we can safely clone.
        let t = pick_template(&native()).unwrap();
        assert_eq!(slug_of(&t), "gpt-5.2");
    }

    #[test]
    fn template_falls_back_when_nothing_is_listed() {
        let hidden_only = vec![json!({"slug": "only", "visibility": "hide", "priority": 5})];
        assert_eq!(slug_of(&pick_template(&hidden_only).unwrap()), "only");
        assert!(pick_template(&[]).is_none());
    }

    #[test]
    fn cloned_entry_keeps_the_schema_but_takes_our_identity() {
        let t = pick_template(&native()).unwrap();
        let e = entry(&t, "claude-fable-5", 0);
        assert_eq!(e["slug"], "claude-fable-5");
        assert_eq!(e["display_name"], "claude-fable-5");
        assert_eq!(e["visibility"], "list");
        assert_eq!(e["priority"], 1, "listed ahead of the hidden natives");
        assert_eq!(e["base_instructions"], "5.2 prompt", "harness prompt is inherited");
        assert_eq!(e["upgrade"], Value::Null, "the template's upgrade banner is not inherited");
    }

    #[test]
    fn parse_models_accepts_both_envelopes() {
        assert_eq!(parse_models(r#"{"models":[{"slug":"a"}]}"#).unwrap().len(), 1);
        assert_eq!(parse_models(r#"[{"slug":"a"}]"#).unwrap().len(), 1);
        assert!(parse_models(r#"{"models":[]}"#).is_none(), "an empty catalog is not usable");
        assert!(parse_models("not json").is_none());
    }

    #[test]
    fn is_ours_only_matches_the_file_we_generate() {
        assert!(is_ours(&path().to_string_lossy()));
        assert!(!is_ours("/Users/someone/.codex/my-own-catalog.json"));
    }
}
