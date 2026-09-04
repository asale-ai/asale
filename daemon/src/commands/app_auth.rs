//! Signing the framed apps in — the desktop shell's half of "Sign in with asale".
//!
//! Studio and Swarm are separate origins in an iframe, with their own storage
//! and no access to this app's session. They used to be handed an API key: the
//! shell read one off the account, posted it across `postMessage`, and the frame
//! spent it. That worked, and it meant a bearer credential crossing a window
//! boundary on every launch, for an app that has a proper way in.
//!
//! This is that way in, and it is exactly what `asale.ai` does for the same two
//! frames (`asale-web/src/components/AppFrame.tsx`):
//!
//!   frame → shell   `{ type: "ready", oauth: { state, code_challenge } }`
//!   shell → asale   POST /api/v1/oauth/authorize   (with this device's session)
//!   shell → frame   `{ type: "oauth_code", code, state }`
//!
//! The frame gets an authorization code bound to a PKCE challenge whose verifier
//! never left it, so the code is redeemable by that frame and by nobody else —
//! this daemon included. Nothing here outlives the round trip.
//!
//! `client_id` is pinned to the app being framed rather than taken from the
//! request. The caller chooses *which of our apps* is asking; it does not get to
//! name a third-party client and have this device's session approve it. The
//! redirect comes from the caller because a dev build frames a local bundle, and
//! it is safe to pass through: the server only accepts a redirect registered for
//! the client_id, which this function is the one deciding.

use super::server_client::authed;
use super::R;
use crate::cmd_err;
use crate::state::AppState;
use serde_json::{json, Value};

/// What the two framed apps are registered as (`oauth_clients`, migrations 0083
/// and 0090). The scope is the same pair the website asks for: `profile` so the
/// app can say who is signed in, `inference` for the gateway key it spends.
fn client_id(app: &str) -> R<&'static str> {
    match app {
        "studio" => Ok("asale-studio"),
        "swarm" => Ok("asale-swarm"),
        // AEO does not ask this shell for a code the way the two bundles do —
        // it runs the whole flow inside its own frame. What it needs is the
        // *consent page* to be answerable without a browser session, which the
        // shell does on its behalf when the framed page asks (`pages/Apps.tsx`).
        "aeo" => Ok("asale-aeo"),
        _ => Err(cmd_err!("errors.oauth.unknownApp", "unknown app")),
    }
}

const SCOPE: &str = "profile inference";

/// Approve an authorization request on behalf of the signed-in device.
///
/// Answers `{ code, state, error }` — the same three fields the frame would have
/// read off a redirect — plus `redirect_to`, the whole URL, for the caller that
/// is going to *navigate* rather than read the fields. A refusal comes
/// back as `error` rather than as a failed command: the frame has a sign-in
/// screen of its own to fall back to, and an error dialog over an iframe that is
/// about to offer a button is noise.
pub async fn app_authorize(
    state: &AppState,
    app: String,
    redirect_uri: String,
    oauth_state: String,
    code_challenge: String,
) -> R<Value> {
    let client_id = client_id(&app)?;
    let v = authed(
        state,
        reqwest::Method::POST,
        "/api/v1/oauth/authorize",
        Some(json!({
            "client_id": client_id,
            "redirect_uri": redirect_uri,
            "response_type": "code",
            "scope": SCOPE,
            "state": oauth_state,
            "code_challenge": code_challenge,
            "code_challenge_method": "S256",
            "approve": true,
        })),
    )
    .await?;

    // The server answers with the URL a browser would have been sent to; only
    // its query is wanted, since the frame is already at the destination.
    let redirect_to = v["redirect_to"].as_str().unwrap_or_default();
    // `reqwest::Url` rather than a `url` dependency of our own: it is the same
    // type, already in the tree.
    let url = reqwest::Url::parse(redirect_to)
        .map_err(|_| cmd_err!("errors.oauth.badRedirect", "authorize answered with an unusable redirect"))?;
    let param = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned()).unwrap_or_default();
    Ok(json!({
        "code": param("code"),
        "state": param("state"),
        "error": param("error"),
        "redirect_to": redirect_to,
    }))
}
