//! The Security page's commands: the firewall policy, its per-tool switches,
//! what it has decided lately, and a scratchpad for trying it out.
//!
//! The policy itself, the compiled scanners and the log live in
//! [`crate::firewall`]; this module is only the shape the page reads and
//! writes.

use crate::firewall;
use crate::state::AppState;
use crate::tool_config;
use serde_json::{json, Value};
use super::{err, R};

/// The whole page in one call: the policy, one row per tool, and the scanner
/// catalogue.
///
/// The scanner list is served rather than hard-coded in the frontend because
/// the *names* are the crate's contract (`Scanner::as_str`) and the counts come
/// from its rule tables — a page that spelled them itself would drift the first
/// time a rule was added.
pub async fn firewall_policy(state: &AppState) -> R<Value> {
    let policy = firewall::policy(&state.store).await;

    let mut tools = Vec::new();
    for id in tool_config::TOOLS {
        let t = policy.tools.get(*id).cloned().unwrap_or_default();
        let id_owned = (*id).to_string();
        let installed = tokio::task::spawn_blocking(move || tool_config::installed(&id_owned))
            .await
            .unwrap_or(false);
        tools.push(json!({
            "id": id,
            "label": tool_config::label(id),
            "installed": installed,
            "enabled": t.enabled,
            "mode": t.mode,
        }));
    }

    Ok(json!({
        "policy": policy,
        "tools": tools,
        "scanners": scanner_catalogue(),
        "log_path": firewall::log_path().to_string_lossy(),
    }))
}

/// Every scanner with its id, the rules behind it and which direction it
/// guards. `rules: 0` means the scanner has no table — its checks are computed
/// (destinations) or its finding is the presence of the thing itself (invisible
/// characters).
fn scanner_catalogue() -> Vec<Value> {
    let tables = agent_firewall::rules::tables();
    let count = |name: &str| {
        tables.iter().find(|(n, _)| *n == name).map(|(_, t)| t.len()).unwrap_or(0)
    };
    agent_firewall::Scanner::ALL
        .iter()
        .map(|s| {
            let id = s.as_str();
            json!({
                "id": id,
                "rules": count(id),
                "direction": match *s {
                    agent_firewall::Scanner::Injection => "inbound",
                    agent_firewall::Scanner::HiddenUnicode => "both",
                    _ => "outbound",
                },
            })
        })
        .collect()
}

/// Turn protection on or off for one tool, and pick how hard it leans.
pub async fn set_firewall_tool(state: &AppState, tool: String, enabled: bool, mode: Option<String>) -> R<Value> {
    if !tool_config::known(&tool) {
        return Err(crate::cmd_err!("errors.firewall.unknownTool", "unknown tool", tool = tool));
    }
    let mode = mode.unwrap_or_else(|| "audit".into());
    if !matches!(mode.as_str(), "audit" | "balanced" | "strict") {
        return Err(crate::cmd_err!("errors.firewall.unknownMode", "unknown firewall mode", mode = mode));
    }
    let policy = firewall::patch(
        &state.store,
        &json!({ "tools": { tool.clone(): { "enabled": enabled, "mode": mode } } }),
    )
    .await
    .map_err(err)?;
    Ok(json!({ "policy": policy }))
}

/// Change anything else on the policy — the five scanner switches, redaction,
/// the audit log, the host lists, the suppressions.
///
/// A free-form patch rather than one command per switch: they are all the same
/// object, the page sends what the user touched, and merging means a field this
/// build does not know about survives being written by one that does.
pub async fn set_firewall_options(state: &AppState, patch: Value) -> R<Value> {
    if !patch.is_object() {
        return Err(crate::cmd_err!("errors.firewall.badPatch", "firewall settings must be an object"));
    }
    // `tools` has its own command, which validates the tool id and the mode.
    // Letting it through here too would be a second, unchecked way in.
    let mut patch = patch;
    if let Some(o) = patch.as_object_mut() {
        o.remove("tools");
    }
    let policy = firewall::patch(&state.store, &patch).await.map_err(err)?;
    Ok(json!({ "policy": policy }))
}

/// What the firewall has decided lately, newest first. Findings are already
/// masked by the scanners, so this is safe to render.
pub async fn firewall_events(limit: Option<usize>) -> R<Value> {
    let limit = limit.unwrap_or(50).clamp(1, 500);
    let events = tokio::task::spawn_blocking(move || firewall::events(limit)).await.map_err(err)?;
    let (records, broken) = tokio::task::spawn_blocking(|| {
        agent_firewall::AuditLog::verify(firewall::log_path()).unwrap_or((0, None))
    })
    .await
    .map_err(err)?;
    Ok(json!({
        "events": events,
        "total": records,
        // Non-null = the log was edited under us, and this is the first record
        // that no longer verifies. Worth showing: the log is evidence about a
        // process that had shell access on this machine.
        "tampered_at": broken,
    }))
}

/// Run one piece of text past the scanners without sending it anywhere.
///
/// The page's "try it" box. Uses the live policy, so what it shows is what the
/// firewall would actually do — a preview built from the defaults would be a
/// demo rather than an answer.
pub async fn firewall_check(state: &AppState, text: String, kind: Option<String>) -> R<Value> {
    use agent_firewall::Subject;
    let engine = firewall::engine(&state.store).await;
    // The scratchpad is not any one tool's traffic, so it is judged at the
    // strictest mode any tool is actually set to — otherwise a user with
    // everything on `strict` would be shown an `audit` answer.
    let mode = engine
        .policy
        .tools
        .values()
        .filter(|t| t.enabled)
        .map(|t| match t.mode.as_str() {
            "strict" => 2,
            "balanced" => 1,
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    let cfg = match mode {
        2 => agent_firewall::Mode::Strict,
        1 => agent_firewall::Mode::Balanced,
        _ => agent_firewall::Mode::Audit,
    };
    let fw = agent_firewall::Firewall::new(agent_firewall::Config {
        mode: cfg,
        ..serde_json::from_value(serde_json::to_value(&engine.policy).map_err(err)?).unwrap_or_default()
    })
    .map_err(|e| err(anyhow::anyhow!(e.to_string())))?;

    let verdict = match kind.as_deref() {
        Some("url") => fw.inspect(&Subject::url(&text)),
        Some("tool_call") => fw.inspect(&Subject::tool_call("bash", &text)),
        Some("prompt") => fw.inspect(&Subject::prompt(&text)),
        // Default: treat it as something that came *back*, which is the case
        // the page exists to explain.
        _ => fw.inspect(&Subject::tool_output(&text)),
    };
    Ok(json!({ "verdict": verdict, "mode": cfg.as_str() }))
}
