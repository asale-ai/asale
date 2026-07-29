//! Task records and reconciliation against the server's authoritative copy.

use crate::state::AppState;
use asale_client_core::reconcile;
use serde_json::{json, Value};
use super::server_client::{authed};
use super::{R, err, now_secs};

/// Task records: the server's authoritative page plus the matching local page
/// (provider_records / consume_records) for offline display and reconcile.
pub async fn records_query(state: &AppState, role: String, page: i64) -> R<Value> {
    let role = if role == "provider" { "provider" } else { "consumer" };
    let page = page.max(1);
    let per_page = 50i64;
    let offset = (page - 1) * per_page;

    let (local_rows, local_total) = if role == "provider" {
        state.store.list_provider_records(per_page, offset).await.map_err(err)?
    } else {
        state.store.list_consume_records(per_page, offset).await.map_err(err)?
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
/// Consumer-side task ids are minted locally (the gateway does not expose its
/// task id to the consumer), so that side reconciles by count.
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

    // Consumer side: count comparison (local ids are client-minted).
    let consumer_server = authed(state, reqwest::Method::GET, "/api/v1/me/records?role=consumer&page=1", None).await;
    let server_count = consumer_server
        .as_ref()
        .ok()
        .and_then(|v| v["records"].as_array().map(|a| a.len()))
        .unwrap_or(0);
    let (_, local_consume_total) = state.store.list_consume_records(1, 0).await.map_err(err)?;

    Ok(json!({
        "provider": summary,
        "synced_amounts": synced,
        "consumer": {"server_page_count": server_count, "local_total": local_consume_total},
        "reconciled_at": now_secs(),
    }))
}
