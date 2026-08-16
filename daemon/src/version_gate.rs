//! Ask the platform what the oldest tradable build is, before anything tries to
//! trade.
//!
//! The gate itself lives on the server and is enforced where the money moves:
//! the publisher's socket is refused, a buy comes back as a sentence telling the
//! user to upgrade. Both of those arrive *inside* something the user was in the
//! middle of — a Codex turn, a sale that silently stopped — and neither of them
//! happens at all until they try. A user who opens the app in the morning to a
//! window that looks completely normal, and only finds out at the first prompt
//! of the day, has been told too late.
//!
//! So the floor is also read directly, on a slow clock, and handed to
//! [`asale_client_core::upgrade`] — the same place the two refusal paths record
//! into, so the window has one thing to render and not three.
//!
//! Failures here are silent on purpose. This is a poll of a remote number; a
//! machine that is offline, behind a captive portal, or pointed at a server
//! mid-deploy learns nothing, and "learned nothing" must never read as "the gate
//! is off" (which would clear a real refusal) or as "you are blocked" (which
//! would lock the window over a dropped packet).

use std::time::Duration;

/// How long after start the first read waits.
///
/// Long enough to be behind the work a cold start actually needs — the store
/// opening, the catalog pull, the first pool rebuild — and short enough that a
/// blocked user sees the dialog while they are still looking at the window they
/// just opened.
const FIRST_DELAY: Duration = Duration::from_secs(3);

/// How often it is re-read afterwards.
///
/// This changes when an operator raises a platform-wide floor, which happens on
/// the order of a release, not of a user action. Ten minutes is the same clock
/// the desktop shell's release check runs on, and the two answer the same
/// question from opposite ends: "is there a newer build?" and "is this one still
/// allowed?".
const INTERVAL: Duration = Duration::from_secs(600);

/// Read the floor once and apply it. `Ok(min)` carries what the server said —
/// `""` meaning the gate is off, which is an answer and not a failure.
pub async fn fetch(api_base: &str) -> anyhow::Result<String> {
    let http = asale_client_core::http::plain();
    let url = format!("{}/api/v1/client/min-version", api_base.trim_end_matches('/'));
    let resp = http.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        // Most likely a server that predates the endpoint. Not an error worth
        // shouting about, but it must not be read as an empty floor either.
        anyhow::bail!("min-version returned {status}");
    }
    let body: serde_json::Value = resp.json().await?;
    let min = body
        .get("min_version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("min-version answered without a min_version field: {body}"))?
        .trim()
        .to_string();
    asale_client_core::upgrade::apply_floor(&min);
    Ok(min)
}

/// Start the loop. Never awaited by anything: the app must not wait on a network
/// round trip to find out it is allowed to run.
pub fn spawn_loop(api_base: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_DELAY).await;
        // Only ever logged when it changes: this runs every ten minutes for the
        // life of the daemon, and a line per read would bury everything else.
        let mut last: Option<String> = None;
        loop {
            match fetch(&api_base).await {
                Ok(min) => {
                    if last.as_deref() != Some(min.as_str()) {
                        match (min.is_empty(), asale_client_core::upgrade::get().is_some()) {
                            (true, _) => tracing::info!("the platform has no minimum client version"),
                            (false, true) => tracing::warn!(
                                min = %min,
                                current = %asale_client_core::http::VERSION,
                                "this build is below the platform's minimum — the app will ask the user to upgrade"
                            ),
                            (false, false) => tracing::info!(min = %min, "platform minimum client version"),
                        }
                        last = Some(min);
                    }
                }
                Err(e) => tracing::debug!("could not read the platform's minimum client version: {e}"),
            }
            tokio::time::sleep(INTERVAL).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;

    /// `upgrade`'s notice is process-global, so these two would otherwise
    /// clear each other's state while running side by side.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A stand-in for the server, answering `body` at the real path.
    async fn serve(body: &'static str, status: u16) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/api/v1/client/min-version",
            get(move || async move {
                (axum::http::StatusCode::from_u16(status).unwrap(), [(axum::http::header::CONTENT_TYPE, "application/json")], body)
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_floor_this_build_is_under_raises_the_dialog_and_a_lower_one_takes_it_away() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let base = serve(r#"{"min_version":"999.0.0"}"#, 200).await;
        assert_eq!(fetch(&base).await.unwrap(), "999.0.0");
        let n = asale_client_core::upgrade::get().expect("blocked");
        assert_eq!(n.path, asale_client_core::upgrade::PATH_PLATFORM);
        assert_eq!(n.current, asale_client_core::http::VERSION);

        let base = serve(r#"{"min_version":""}"#, 200).await;
        assert_eq!(fetch(&base).await.unwrap(), "");
        assert!(asale_client_core::upgrade::get().is_none(), "an empty floor is the gate being off");
    }

    /// The case that must never read as "the gate is off": a server that has
    /// never heard of this endpoint would otherwise clear a real refusal.
    #[tokio::test]
    async fn a_server_without_the_endpoint_changes_nothing() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        asale_client_core::upgrade::record("999.0.0", "sell");
        let base = serve("", 404).await;
        assert!(fetch(&base).await.is_err());
        assert!(asale_client_core::upgrade::get().is_some(), "the publisher's refusal survives");
        asale_client_core::upgrade::clear();
    }
}
