//! Task records and reconciliation against the server's authoritative copy.

use crate::state::AppState;
use asale_client_core::reconcile;
use serde_json::{json, Value};
use super::server_client::{authed};
use super::{R, err, now_secs};

/// Task records: the server's authoritative page, plus — on the provider side —
/// the matching local page for offline display and reconcile.
///
/// There is no consumer-side local page. This device serves the calls it sells,
/// so `provider_records` is a first-hand account with real settled amounts in
/// it; the calls it buys are priced at settlement, after its stream has closed,
/// so the rows it used to keep for those carried a zero amount and token counts
/// taken off the relayed response rather than off the bill. An offline consumer
/// gets `server_error` and no table, which is the honest answer.
pub async fn records_query(state: &AppState, role: String, page: i64) -> R<Value> {
    let role = if role == "provider" { "provider" } else { "consumer" };
    let page = page.max(1);
    let per_page = 50i64;
    let offset = (page - 1) * per_page;

    let (local_rows, local_total) = if role == "provider" {
        state.store.list_provider_records(per_page, offset).await.map_err(err)?
    } else {
        (Vec::new(), 0)
    };

    let server = authed(state, reqwest::Method::GET, &format!("/api/v1/me/records?role={role}&page={page}"), None).await;
    let (records, server_error) = match server {
        Ok(v) => (v["records"].clone(), Value::Null),
        Err(e) => (json!([]), json!(e.message)),
    };

    Ok(json!({
        "role": role,
        "page": page,
        "per_page": per_page,
        "records": records,
        "server_error": server_error,
        "local": {"records": local_rows, "total": local_total},
    }))
}

/// Manual reconciliation (spec §8.1): pull the server's provider-side records,
/// diff against local `provider_records` by task_id, sync settled amounts
/// locally (server is authoritative) and return the difference summary.
///
/// Only the provider side has two copies to reconcile. The consumer side used to
/// compare counts — the one thing it could compare, since its task ids were
/// minted locally and never matched the gateway's — which made it a check that
/// could fail for reasons nobody could act on. It now has a single copy, the
/// server's, so there is nothing to diff.
pub async fn reconcile_now(state: &AppState) -> R<Value> {
    // Server truth: up to 3 pages (150 newest provider-side records).
    let mut server_entries: Vec<reconcile::RecEntry> = Vec::new();
    for page in 1..=3 {
        let v = authed(state, reqwest::Method::GET, &format!("/api/v1/me/records?role=provider&page={page}"), None).await?;
        let rows = v["records"].as_array().cloned().unwrap_or_default();
        let n = rows.len();
        for r in rows {
            server_entries.push(reconcile::RecEntry {
                task_id: r["task_id"].as_str().unwrap_or_default().to_string(),
                tokens: r["in_tokens"].as_i64().unwrap_or(0) + r["out_tokens"].as_i64().unwrap_or(0),
                // What this device earned — the provider-side ledger value.
                amount: r["provider_income"].as_i64().unwrap_or(0),
            });
        }
        if n < 50 {
            break;
        }
    }

    let (local_rows, _) = state.store.list_provider_records(500, 0).await.map_err(err)?;
    let local_entries: Vec<reconcile::RecEntry> = local_rows
        .iter()
        .map(|r| reconcile::RecEntry {
            task_id: r.task_id.clone(),
            tokens: r.in_tokens + r.out_tokens,
            amount: r.amount_usdt,
        })
        .collect();

    let (summary, fixes) = reconcile::diff(&local_entries, &server_entries);
    // Server wins: sync settled amounts into the local ledger.
    let synced = fixes.len();
    for fix in fixes {
        state.store.set_provider_record_amount(&fix.task_id, fix.server_amount).await.map_err(err)?;
    }

    Ok(json!({
        "provider": summary,
        "synced_amounts": synced,
        "reconciled_at": now_secs(),
    }))
}
