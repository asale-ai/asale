//! Local AI-CLI usage scanner — the "我使用的" (used) source for the Usage page.
//!
//! Parses Claude Code session logs (`~/.claude/projects/**/*.jsonl`) the same
//! way TokenTracker does: each `type:"assistant"` line carries `message.usage`
//! with input / output / cache-creation / cache-read token counts. Totals fold
//! into the `usage_daily` snapshot under `source="used"`, aggregated by UTC day
//! and model. Reads only token counts + timestamps — never prompt/response text.
//!
//! Incremental: each file keeps a byte-offset cursor (jsonl is append-only), so
//! a scan only reads bytes appended since last time. Dedup follows Anthropic's
//! protocol — `message.id` is globally unique per response (with `requestId`
//! appended when present) — to avoid the 1.6–3.7× overcount retries/sub-agents
//! would otherwise cause.

use asale_client_core::store::LocalStore;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Default)]
struct Bucket {
    in_tokens: i64,
    out_tokens: i64,
    cache: i64,
    cnt: i64,
}

/// Scan local CLI logs and fold new usage into `usage_daily` (source="used").
/// Returns the number of assistant messages folded this pass.
pub async fn scan_claude_logs(store: &LocalStore) -> anyhow::Result<u64> {
    let Some(home) = std::env::var_os("HOME") else { return Ok(0) };
    let root = PathBuf::from(home).join(".claude").join("projects");
    if !root.is_dir() {
        return Ok(0);
    }

    let mut files = Vec::new();
    collect_jsonl(&root, &mut files);

    let mut buckets: HashMap<(String, String), Bucket> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut folded = 0u64;

    for path in files {
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let size = meta.len() as i64;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let path_str = path.to_string_lossy().to_string();

        let (stored_offset, _stored_mtime) = store.get_scan_offset(&path_str).await?;
        // Append-only: start where we left off. If the file shrank it was
        // rewritten — re-read from the start.
        let mut offset = if stored_offset <= size { stored_offset } else { 0 };
        if size == offset {
            continue; // nothing new
        }

        let mut f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if f.seek(SeekFrom::Start(offset as u64)).is_err() {
            continue;
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            continue; // non-UTF8 / read error — skip this pass
        }

        for line in buf.split_inclusive('\n') {
            if !line.ends_with('\n') {
                break; // trailing partial line (file mid-write) — leave for next pass
            }
            offset += line.len() as i64;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some((day, model, b)) = parse_line(trimmed, &mut seen) {
                let e = buckets.entry((day, model)).or_default();
                e.in_tokens += b.in_tokens;
                e.out_tokens += b.out_tokens;
                e.cache += b.cache;
                e.cnt += 1;
                folded += 1;
            }
        }
        store.set_scan_offset(&path_str, offset, mtime).await?;
    }

    for ((day, model), b) in buckets {
        store.add_usage("used", &day, &model, b.in_tokens, b.out_tokens, b.cache, b.cnt, 0).await?;
    }
    Ok(folded)
}

/// Parse one assistant jsonl line into `(day, model, bucket)`, honoring dedup.
/// Returns `None` for non-assistant lines, missing usage, or duplicates.
fn parse_line(line: &str, seen: &mut HashSet<String>) -> Option<(String, String, Bucket)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|x| x.as_str()) != Some("assistant") {
        return None;
    }
    let msg = v.get("message")?;
    let usage = msg.get("usage").filter(|u| u.is_object())?;

    // Dedup by message.id (+ requestId when present); count when no id.
    if let Some(msg_id) = msg.get("id").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
        let key = match v.get("requestId").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
            Some(req) => format!("{msg_id}:{req}"),
            None => msg_id.to_string(),
        };
        if !seen.insert(key) {
            return None; // already counted
        }
    }

    let ts = v.get("timestamp").and_then(|x| x.as_str())?;
    if ts.len() < 10 {
        return None;
    }
    let day = ts[..10].to_string(); // 'YYYY-MM-DD' (timestamps are UTC 'Z')
    let model = msg
        .get("model")
        .and_then(|x| x.as_str())
        .filter(|m| !m.is_empty() && !m.starts_with('<'))
        .unwrap_or("unknown")
        .to_string();

    let n = |k: &str| usage.get(k).and_then(|x| x.as_i64()).unwrap_or(0).max(0);
    let in_tokens = n("input_tokens");
    let out_tokens = n("output_tokens");
    let cache = n("cache_creation_input_tokens") + n("cache_read_input_tokens");
    Some((day, model, Bucket { in_tokens, out_tokens, cache, cnt: 1 }))
}

/// Recursively collect `*.jsonl` files under `dir`.
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_jsonl(&p, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}
