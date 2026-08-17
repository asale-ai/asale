//! The model catalog Codex shows in its picker.
//!
//! Codex does not ask a provider which models it serves — no `/v1/models` call
//! is ever made. Its picker is fed by a catalog compiled into the binary
//! (`codex debug models`), which is why a model bought on asale would otherwise
//! never appear in the ChatGPT app's model list no matter what the buy page is
//! set to. The one supported override is the `model_catalog_json` config key:
//! a path to a JSON document that *replaces* the built-in catalog wholesale.
//!
//! ## Why most entries wear OpenAI's slugs
//!
//! Overriding the catalog is only half the battle. The ChatGPT desktop app's
//! renderer takes the list its own app-server just returned and filters it
//! *again*. That filter reads (deminified from the shipped bundle):
//!
//! ```js
//! additionalAvailableModels?.has(m.model) === true ||
//!   (m.model !== "codex-auto-review" &&
//!     (useHiddenModels && !isCustomModelProvider && authMethod !== "amazonBedrock"
//!       ? availableModels.has(m.model)   // ← allowlist from OpenAI's servers
//!       : !m.hidden))                    // ← everything the catalog lists
//! ```
//!
//! `availableModels` is an allowlist of slugs delivered from OpenAI's servers
//! (a Statsig dynamic config carrying `available_models` / `use_hidden_models`),
//! and a slug the market invented is not on it — the picker drops it and the
//! composer falls back to "Custom" (upstream bug openai/codex#19694). But that
//! branch is only taken when the config has *no* custom provider, and asale
//! always writes one (`model_provider = "asale"` plus the matching
//! `[model_providers.asale]`, which is exactly what the app tests for). With
//! the switch on, the app is therefore in the `!m.hidden` branch: it lists
//! whatever our catalog lists, invented slugs included.
//!
//! We still hand out native slugs first, because that path needs no such
//! reasoning — a rewritten native entry is browsable on any build, allowlist or
//! not — and only synthesize entries once the natives run out. So each selected
//! model is published as, in order of preference:
//!   1. *itself*, when the market id is already a native slug (`gpt-5.6-sol`);
//!   2. a **carrier** — a native entry rewritten in place, slug kept as
//!      `gpt-5.5`, display name becoming the model the user bought;
//!   3. a **synthesized** entry: a native one cloned under the market id as its
//!      slug, which only the custom-provider branch above will show.
//!
//! The slug → market model mapping is recorded in [`aliases_path`] so the local
//! proxy can translate requests back on the way out (see `proxy::buy_gate`).
//! Every entry not carrying a selected model is forced to `visibility: "hide"`,
//! so the app's internal lookups (auto-review and friends) still resolve while
//! the picker stops offering models the market cannot serve.
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
    crate::state::home_dir().unwrap_or_else(|| PathBuf::from("."))
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
    let publish = assign(&native, models);
    // `assign` gives every model a slug, so this only trips on a catalog with
    // no entry at all to clone — in which case there is nothing to publish.
    let Some(start_slug) = publish.first().map(|(slug, _)| slug.clone()) else {
        tracing::warn!("codex model catalog has no entry that can carry a market model");
        return Ok(None);
    };
    // slug → the model it publishes, and the picker position it takes.
    let order: BTreeMap<&str, (usize, &str)> = publish
        .iter()
        .enumerate()
        .map(|(i, (slug, model))| (slug.as_str(), (i, model.as_str())))
        .collect();

    let mut entries: Vec<Value> = Vec::new();
    for m in native.iter() {
        match order.get(slug_of(m)) {
            Some((i, model)) => entries.push(carrier(m.clone(), model, *i)),
            None => entries.push(hidden(m.clone())),
        }
    }
    // Whatever no native entry could publish gets one cloned for it, so the
    // schema (~30 fields, harness prompt included) is never hand-authored.
    let native_slugs: Vec<&str> = native.iter().map(slug_of).collect();
    for (i, (slug, model)) in publish.iter().enumerate() {
        if native_slugs.contains(&slug.as_str()) {
            continue;
        }
        let Some(t) = template(&native) else { break };
        let mut e = t.clone();
        if let Some(o) = e.as_object_mut() {
            o.insert("slug".into(), json!(slug));
        }
        entries.push(carrier(e, model, i));
    }

    let assigned: BTreeMap<&str, &str> =
        publish.iter().map(|(slug, model)| (slug.as_str(), model.as_str())).collect();
    let target = path();
    write_atomic(&target, &serde_json::to_string_pretty(&json!({ "models": entries }))?)?;
    write_atomic(
        &aliases_path(),
        &serde_json::to_string_pretty(&Value::Object(
            assigned.iter().map(|(s, m)| ((*s).to_string(), json!(m))).collect::<Map<_, _>>(),
        ))?,
    )?;
    Ok(Some(Generated { path: target, start_slug }))
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

/// The slug each selected model is published under, in picker order.
///
/// Preference order per model — see the module docs for why: its own slug when
/// the market id is one Codex already knows, then an unclaimed carrier, then
/// the market id as a slug of its own. Every result is distinct: carriers that
/// a model will claim as *itself* are held back, so the two can never collide,
/// and the market ids are unique to begin with.
fn assign(native: &[Value], models: &[String]) -> Vec<(String, String)> {
    let is_native = |slug: &str| native.iter().any(|m| slug_of(m) == slug);
    let mut carriers = pick_carriers(native)
        .into_iter()
        .filter(|slug| !models.iter().any(|m| m == slug))
        .collect::<Vec<_>>()
        .into_iter();
    models
        .iter()
        .map(|model| {
            let slug = if is_native(model) {
                model.clone()
            } else {
                carriers.next().unwrap_or_else(|| model.clone())
            };
            (slug, model.clone())
        })
        .collect()
}

/// The entry a synthesized one is cloned from: the best carrier if there is
/// one, so the clone inherits a shape the app is known to render, and any entry
/// at all otherwise — a catalog we could not recognize still beats none.
fn template(native: &[Value]) -> Option<&Value> {
    let best = pick_carriers(native).into_iter().next();
    best.and_then(|slug| native.iter().find(|m| slug_of(m) == slug)).or_else(|| native.first())
}

/// The native entries a market model may be published under *someone else's*
/// name, best first. An entry publishing itself goes through [`assign`] and is
/// held to none of this — nothing is being disguised there.
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

/// An entry wearing a market model's identity. Its `slug` is left exactly as
/// the caller set it — a native one it keeps, or the market id a synthesized
/// entry was cloned under (see the module docs).
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

    fn ids(models: &[&str]) -> Vec<String> {
        models.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_selection_longer_than_the_carriers_still_gets_published_in_full() {
        // The two carriers go first — they are browsable whatever the app's
        // allowlist says — and the rest take slugs of their own rather than
        // being dropped, which is what left the picker showing a prefix.
        assert_eq!(
            assign(&native(), &ids(&["claude-opus-5", "claude-sonnet-5", "claude-haiku-5"])),
            [
                ("gpt-5.5".into(), "claude-opus-5".to_string()),
                ("gpt-5.2".into(), "claude-sonnet-5".to_string()),
                ("claude-haiku-5".into(), "claude-haiku-5".to_string()),
            ]
        );
    }

    #[test]
    fn a_model_codex_already_knows_is_published_as_itself() {
        // sol is no carrier — but publishing it under its own slug disguises
        // nothing, and it leaves gpt-5.5 free for the model that needs one.
        assert_eq!(
            assign(&native(), &ids(&["gpt-5.6-sol", "claude-opus-5"])),
            [
                ("gpt-5.6-sol".into(), "gpt-5.6-sol".to_string()),
                ("gpt-5.5".into(), "claude-opus-5".to_string()),
            ]
        );
        // And a carrier a later model will claim as itself is never handed to
        // an earlier one, which would leave both wanting the same slug.
        assert_eq!(
            assign(&native(), &ids(&["claude-opus-5", "gpt-5.5"])),
            [
                ("gpt-5.2".into(), "claude-opus-5".to_string()),
                ("gpt-5.5".into(), "gpt-5.5".to_string()),
            ]
        );
    }

    #[test]
    fn a_synthesized_entry_is_cloned_from_the_best_carrier() {
        assert_eq!(slug_of(template(&native()).unwrap()), "gpt-5.5");
        assert!(template(&[]).is_none(), "nothing to clone from an empty catalog");
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
        // Both sides of the first assertion read `$HOME`, which is
        // process-global: without the lock a test that repoints it in between
        // makes this compare two different homes and fail for a reason that has
        // nothing to do with `is_ours`.
        let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(is_ours(&path().to_string_lossy()));
        assert!(!is_ours("/Users/someone/.codex/my-own-catalog.json"));
    }
}
