//! The model catalog Codex shows in its picker.
//!
//! Codex does not ask a provider which models it serves — no `/v1/models` call
//! is ever made. Its picker is fed by a catalog compiled into the binary
//! (`codex debug models`), which is why a model bought on asale would otherwise
//! never appear in the ChatGPT app's model list no matter what the buy page is
//! set to. The one supported override is the `model_catalog_json` config key:
//! a path to a JSON document that *replaces* the built-in catalog wholesale.
//!
//! ## Why the entries wear OpenAI's slugs
//!
//! Overriding the catalog is only half the battle. The ChatGPT desktop app's
//! renderer takes the list its own app-server just returned and filters it
//! *again*, against an allowlist of slugs delivered from OpenAI's servers
//! (a Statsig dynamic config carrying `available_models` / `use_hidden_models`).
//! A slug the market invented is not on that list, so it is dropped before the
//! picker ever sees it: the app falls back to labelling the composer "Custom"
//! and offers nothing to switch to. This is upstream bug openai/codex#19694,
//! still open — restarting the app cannot help, the allowlist is refetched
//! every launch.
//!
//! So the catalog we generate does not introduce slugs. It takes native entries
//! that are already on that allowlist and *rewrites them in place* — the slug
//! stays `gpt-5.5`, the display name becomes the model the user bought — and
//! records the slug → market model mapping in [`aliases_path`] so the local
//! proxy can translate requests back on the way out (see `proxy::buy_gate`).
//! Everything else is forced to `visibility: "hide"`, so the app's internal
//! lookups (auto-review and friends) still resolve while the picker stops
//! offering models the market cannot serve.
//!
//! Rewriting rather than hand-authoring matters for a second reason: the entry
//! schema is large (~30 fields, including the harness prompt) and unversioned.
//! If no codex binary can be found to dump the native catalog, we write no
//! catalog at all and leave the picker alone — `model` in config.toml still
//! points at the bought model, so the switch works, it just is not browsable.

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long `codex debug models` may take before we give up on it.
const DUMP_TIMEOUT: Duration = Duration::from_secs(20);

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// Every path here derives from `$HOME`, which is process-global: tests that
/// repoint it — or that read what lives under it, like [`alias_for`] — hold
/// this so they cannot see each other's sandbox.
#[cfg(test)]
pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where the generated catalog lives. Kept in asale's own directory, not in
/// `~/.codex`, so a restore never has to guess whether a file there was ours.
pub fn path() -> PathBuf {
    home().join(".asale").join("codex-models.json")
}

/// Where the slug → market model mapping lives, beside the catalog it belongs
/// to. Written and removed with it, never separately.
pub fn aliases_path() -> PathBuf {
    home().join(".asale").join("codex-aliases.json")
}

/// A catalog that was generated, and what config.toml has to say about it.
pub struct Generated {
    /// The document to point `model_catalog_json` at.
    pub path: PathBuf,
    /// The slug to put in `model` — the carrier of the first selected model,
    /// *not* the market model id, since that is what Codex will send us.
    pub start_slug: String,
}

/// Generate the catalog for `models` (market model ids, in picker order).
///
/// `Ok(None)` means no catalog could be built — the caller must then leave
/// `model_catalog_json` unset rather than pointing Codex at a file we could not
/// validate.
pub fn write(models: &[String]) -> Result<Option<Generated>> {
    if models.is_empty() {
        return Ok(None);
    }
    let Some(native) = native_catalog() else {
        tracing::warn!("no codex binary found to dump a model catalog; the picker will not list asale models");
        return Ok(None);
    };
    let carriers = pick_carriers(&native);
    if carriers.is_empty() {
        tracing::warn!("codex model catalog has no entry that can carry a market model");
        return Ok(None);
    }
    // One carrier per selected model. A selection longer than the native
    // catalog cannot be shown in full, and saying so beats a picker that
    // silently lists a prefix.
    if carriers.len() < models.len() {
        tracing::warn!(
            "codex can only offer {} of the {} selected models in its picker ({} not shown: {})",
            carriers.len(),
            models.len(),
            models.len() - carriers.len(),
            models[carriers.len()..].join(", ")
        );
    }
    let assigned: BTreeMap<&str, &str> = carriers
        .iter()
        .zip(models.iter())
        .map(|(slug, model)| (slug.as_str(), model.as_str()))
        .collect();

    let mut entries: Vec<Value> = Vec::new();
    for m in native {
        match assigned.get(slug_of(&m)) {
            Some(model) => {
                let order = carriers.iter().position(|s| s == slug_of(&m)).unwrap_or(0);
                entries.push(carrier(m, model, order));
            }
            None => entries.push(hidden(m)),
        }
    }

    let target = path();
    write_atomic(&target, &serde_json::to_string_pretty(&json!({ "models": entries }))?)?;
    write_atomic(
        &aliases_path(),
        &serde_json::to_string_pretty(&Value::Object(
            assigned.iter().map(|(s, m)| ((*s).to_string(), json!(m))).collect::<Map<_, _>>(),
        ))?,
    )?;
    Ok(Some(Generated { path: target, start_slug: carriers[0].clone() }))
}

/// The market model a slug Codex asked for stands for, if it is one of ours.
///
/// Read straight off disk on every call rather than cached: the file is a few
/// hundred bytes, it is read once per proxied request (which is about to spend
/// seconds upstream anyway), and going back to the file is what guarantees the
/// mapping can never drift from the catalog sitting next to it.
pub fn alias_for(slug: &str) -> Option<String> {
    if slug.is_empty() {
        return None;
    }
    let raw = std::fs::read_to_string(aliases_path()).ok()?;
    serde_json::from_str::<Value>(&raw).ok()?.get(slug)?.as_str().map(String::from)
}

/// Drop the generated catalog and its alias table. Called when the switch goes
/// off, so nothing we wrote outlives the switch.
pub fn remove() {
    let _ = std::fs::remove_file(path());
    let _ = std::fs::remove_file(aliases_path());
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
/// It also keeps the dump native — our own `model_catalog_json` is not in
/// scope there, so re-applying never feeds a previous generation back in.
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

/// The native entries a market model may be published under, best first.
///
/// Everything here is read off the dump rather than named, so a new OpenAI
/// generation changes which slugs get used without changing this code:
///   - only entries the picker already lists qualify — a slug OpenAI hides is
///     a slug their allowlist need not carry either;
///   - `use_responses_lite` entries are skipped. Their slimmed-down request
///     shape is tied to one OpenAI model generation, and it is that same
///     current-generation set the desktop app labels from its own compiled-in
///     "power slider" table — a label our `display_name` cannot override.
fn pick_carriers(models: &[Value]) -> Vec<String> {
    let mut usable: Vec<&Value> = models
        .iter()
        .filter(|m| {
            m.get("visibility").and_then(Value::as_str) == Some("list")
                && m.get("use_responses_lite").and_then(Value::as_bool) != Some(true)
                && !slug_of(m).is_empty()
        })
        .collect();
    usable.sort_by_key(|m| m.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX));
    usable.into_iter().map(|m| slug_of(m).to_string()).collect()
}

/// A native entry, kept for internal lookups but out of the picker.
fn hidden(mut entry: Value) -> Value {
    if let Some(o) = entry.as_object_mut() {
        o.insert("visibility".into(), json!("hide"));
    }
    entry
}

/// A native entry wearing a market model's identity. Its `slug` is deliberately
/// left alone — that is the whole point (see the module docs).
fn carrier(mut entry: Value, model: &str, order: usize) -> Value {
    let Some(o) = entry.as_object_mut() else { return entry };
    set(o, "display_name", json!(model));
    set(o, "description", json!("Bought through the Asale market."));
    set(o, "visibility", json!("list"));
    set(o, "supported_in_api", json!(true));
    // Ahead of every hidden native entry, in the order the buy page lists them.
    set(o, "priority", json!(order as i64 + 1));
    // Upgrade banners and first-run nudges belong to the model we replaced.
    set(o, "upgrade", Value::Null);
    set(o, "availability_nux", Value::Null);
    entry
}

fn set(o: &mut Map<String, Value>, key: &str, value: Value) {
    o.insert(key.to_string(), value);
}

fn write_atomic(path: &Path, body: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("catalog");
    let tmp = dir.join(format!("{name}.asale-tmp-{}", std::process::id()));
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
            json!({"slug": "gpt-5.5", "visibility": "list", "priority": 7,
                   "use_responses_lite": false, "base_instructions": "5.5 prompt"}),
            json!({"slug": "codex-auto-review", "visibility": "hide", "priority": 43}),
        ]
    }

    #[test]
    fn carriers_are_listed_non_lite_natives_in_priority_order() {
        // sol has the better priority but a request shape tied to its own
        // generation; the hidden entry is not something the allowlist carries.
        assert_eq!(pick_carriers(&native()), ["gpt-5.5", "gpt-5.2"]);
        assert!(pick_carriers(&[]).is_empty());
    }

    #[test]
    fn a_carrier_keeps_its_slug_but_takes_our_identity() {
        let e = carrier(native()[1].clone(), "claude-fable-5", 0);
        assert_eq!(e["slug"], "gpt-5.2", "the slug is what gets past the app's allowlist");
        assert_eq!(e["display_name"], "claude-fable-5");
        assert_eq!(e["visibility"], "list");
        assert_eq!(e["priority"], 1, "listed ahead of the hidden natives");
        assert_eq!(e["base_instructions"], "5.2 prompt", "harness prompt is inherited");
        assert_eq!(e["upgrade"], Value::Null, "the replaced model's upgrade banner is not");
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
