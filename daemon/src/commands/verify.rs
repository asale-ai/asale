//! Model verification, from the seller's side.
//!
//! # This file decides nothing
//!
//! Every command here forwards to the server and returns what it says. That is
//! not laziness — it is the only arrangement that works. The thing being
//! verified is *this program*: whether the lane it publishes really reaches the
//! model it claims. A check the daemon performed, or a verdict the daemon was
//! allowed to influence, is a check whoever forked the daemon simply deletes,
//! and the fork is the attack.
//!
//! So the split is: the server probes, scores and decides; the client starts
//! the run and shows the result. What the client contributes is the part the
//! server cannot — telling the person at the keyboard what is happening to
//! their account.
//!
//! # Why the sell switch waits on this
//!
//! Going on the market is the moment a seller's subscription starts serving
//! strangers' prompts, and it is the last moment before money is involved. It
//! is also the only moment the seller is actually looking. Everything after it
//! — the sampling, the re-checks, the expiry — happens with nobody watching, so
//! this is where it gets explained.

use super::server_client::authed;
use super::{R, err};
use crate::state::AppState;
use serde_json::{json, Value};

/// Begin verifying one `(account, model)` lane.
///
/// Returns `{job_id}` immediately; the run takes tens of seconds and is
/// followed with [`lane_verification_job`].
pub async fn start_lane_verification(
    state: &AppState,
    provider: String,
    model: String,
) -> R<Value> {
    let device_id = state.device_id.clone();
    authed(
        state,
        reqwest::Method::POST,
        "/api/v1/me/lanes/verify",
        Some(json!({
            "device_id": device_id,
            "provider": provider,
            "model": model,
        })),
    )
    .await
}

/// Where a run has got to.
///
/// Polled by the dialog. Cheap on purpose — the expensive half is the probing,
/// which is happening on the server whether anybody is watching or not, so a
/// closed window costs the run nothing and reopening it resumes the display.
pub async fn lane_verification_job(state: &AppState, job_id: String) -> R<Value> {
    if job_id.is_empty() {
        return Err(err("missing job id"));
    }
    authed(
        state,
        reqwest::Method::GET,
        &format!("/api/v1/me/lanes/verify/{job_id}"),
        None,
    )
    .await
}

/// Every lane's standing on this account, plus whether the gate is enforcing.
///
/// The sell page reads this to draw each lane's badge and to decide whether the
/// confirm button is available. `enforced: false` means the platform is
/// measuring but not yet refusing — the client must not tell a seller their
/// lane cannot sell when it is, in fact, selling.
pub async fn lane_verification_overview(state: &AppState) -> R<Value> {
    let mut v = authed(state, reqwest::Method::GET, "/api/v1/me/lanes/verification", None).await?;
    // Narrowed to this machine's lanes before the page ever sees them.
    //
    // The endpoint answers for the whole account, which is right for a web
    // console and wrong here: the sell page is about the subscriptions
    // attached to *this* computer, and it matches a verdict to a row by
    // `(provider, model)` alone. A seller running two machines would therefore
    // have the other one's verdict rendered against this one's lane — and in
    // the pre-sale dialog that means the confirm button unlocking on a pass
    // this lane never earned. The gateway is unaffected either way, since its
    // own gate is keyed on the full lane, so what this fixes is the page
    // telling the truth rather than a bypass.
    if let Some(lanes) = v.get_mut("lanes").and_then(|l| l.as_array_mut()) {
        lanes.retain(|l| l.get("device_id").and_then(|d| d.as_str()) == Some(state.device_id.as_str()));
    }
    Ok(v)
}

/// The probes behind one verdict.
///
/// Fetched only when the seller opens the evidence view: a report is tens of
/// kilobytes and the overview above is on a poll.
pub async fn lane_verification_report(
    state: &AppState,
    provider: String,
    model: String,
) -> R<Value> {
    let device_id = state.device_id.clone();
    authed(
        state,
        reqwest::Method::POST,
        "/api/v1/me/lanes/verification/report",
        Some(json!({
            "device_id": device_id,
            "provider": provider,
            "model": model,
        })),
    )
    .await
}
