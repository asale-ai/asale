//! The web UI is behind the daemon token, not just the API.
//!
//! `asale open` hands the browser a URL carrying the token. Deleting the token
//! from that URL used to leave a perfectly working page: the RPC layer refused
//! the stranger, but the application itself — the account names it renders, the
//! shape of the API behind it — was served to anyone who could reach the port.
//! These tests drive the real router the daemon serves and pin the gate shut.
//!
//! `AppState::new()` touches `$HOME` and `$ASALE_DATA_DIR`, so both are pointed
//! at a throwaway directory first.

use asale_daemon::{rpc, state::AppState};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const TOKEN: &str = "test-daemon-token";

/// `$HOME` and `$ASALE_DATA_DIR` are process-global, and every test here builds
/// its own `AppState` on top of them; run them one at a time.
static ENV: Mutex<()> = Mutex::new(());

struct Sandbox {
    dir: std::path::PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("asale-ui-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::env::set_var("HOME", dir.join("home"));
        std::env::set_var("ASALE_DATA_DIR", dir.join("data"));
        std::env::set_var("ASALE_DISABLE_OS_KEYCHAIN_SCAN", "1");
        Sandbox { dir }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn router() -> axum::Router {
    let state = Arc::new(AppState::new().await.expect("app state"));
    rpc::router(rpc::Ctx { state, token: Arc::new(TOKEN.to_string()) })
}

async fn get(app: &axum::Router, uri: &str, cookie: Option<&str>) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut req = Request::builder().method("GET").uri(uri);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    send(app, req.body(Body::empty()).unwrap()).await
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), 4 << 20).await.unwrap();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

/// The cookie value a browser would keep out of a `Set-Cookie`.
fn jar(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get(header::SET_COOKIE)?.to_str().ok()?;
    Some(raw.split(';').next()?.to_string())
}

/// The regression this gate exists for: take the URL `asale open` produced,
/// delete the token, and the app must not come back.
#[tokio::test(flavor = "current_thread")]
async fn the_app_is_not_served_to_a_browser_that_never_showed_the_token() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("stripped");
    let app = router().await;

    for path in ["/", "/index.html", "/settings", "/assets/index.js"] {
        let (status, _, body) = get(&app, path, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} was served unguarded");
        // Not merely a refusal — it must not be the application. The bundle
        // boots itself from a module script; the unlock page has neither.
        assert!(!body.contains("<script type=\"module\""), "{path} returned the app bundle");
        assert!(body.contains("locked"), "{path} did not return the unlock page");
    }

    // A wrong token is the same answer as none, and still no application.
    let (status, _, body) = get(&app, "/?token=not-the-token", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!body.contains("<script type=\"module\""));

    // A cookie the browser made up on its own is not a session either.
    let (status, ..) = get(&app, "/", Some("asale_session=not-the-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// And the flow that is supposed to work still does: the tokenized URL trades
/// the token for a session, and the session serves the app.
#[tokio::test(flavor = "current_thread")]
async fn the_tokenized_url_unlocks_the_browser_and_then_leaves_the_url_clean() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("unlock");
    let app = router().await;

    let (status, headers, _) = get(&app, &format!("/?token={TOKEN}"), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // The token must not survive in the address bar: the redirect target puts
    // it in the fragment, which is not sent to any server and which the
    // frontend scrubs as soon as it has read it.
    let location = headers.get(header::LOCATION).unwrap().to_str().unwrap();
    assert!(!location.contains('?'), "token left in the query: {location}");
    assert_eq!(location, format!("/#token={TOKEN}"));

    let cookie = jar(&headers).expect("no session cookie handed out");
    let set = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"), "{set}");

    // With the cookie the app is served — and RPC accepts the same cookie, so a
    // browser that was unlocked at the gate is not then refused by the API.
    let (status, ..) = get(&app, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, ..) = send(&app, rpc_req(Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK, "the gate let the browser in and the API threw it out");

    // …and only that cookie. Accepting the session must not have widened /rpc
    // into something a tokenless caller can reach.
    let (status, ..) = send(&app, rpc_req(None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, ..) = send(&app, rpc_req(Some("asale_session=not-the-token"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// An /rpc call as axum's server would deliver it — `ConnectInfo` comes from the
/// listener, and the handler extracts it, so `oneshot` has to supply it here.
fn rpc_req(cookie: Option<&str>) -> Request<Body> {
    let peer: std::net::SocketAddr = "127.0.0.1:54321".parse().unwrap();
    let mut req = Request::builder()
        .method("POST")
        .uri("/rpc/daemon_info")
        .header(header::CONTENT_TYPE, "application/json")
        .extension(axum::extract::ConnectInfo(peer));
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    req.body(Body::from("{}")).unwrap()
}

/// The unlock page's own handshake — what it posts after reading `#token=`,
/// which the gate itself never sees.
#[tokio::test(flavor = "current_thread")]
async fn the_unlock_handshake_only_accepts_this_daemons_token() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("handshake");
    let app = router().await;

    let post = |body: String| {
        let app = app.clone();
        async move {
            let req = Request::builder()
                .method("POST")
                .uri("/ui-session")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap();
            send(&app, req).await
        }
    };

    let (status, headers, _) = post(format!(r#"{{"token":"{TOKEN}"}}"#)).await;
    assert_eq!(status, StatusCode::OK);
    let cookie = jar(&headers).expect("no session cookie handed out");
    assert_eq!(cookie, format!("asale_session={TOKEN}"));

    for bad in [r#"{"token":"wrong"}"#, r#"{"token":""}"#, "{}"] {
        let (status, headers, _) = post(bad.to_string()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "accepted {bad}");
        assert!(headers.get(header::SET_COOKIE).is_none(), "handed out a session for {bad}");
    }

    // The session the app is served with unlocks the app, and nothing else does.
    let (status, ..) = get(&app, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
}

/// Liveness stays open on purpose: `asale open` probes it to tell "the port is
/// dead, start the service" from "the service is up and wants the token", and
/// it carries nothing worth guarding.
#[tokio::test(flavor = "current_thread")]
async fn liveness_is_still_answerable_without_a_token() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("liveness");
    let app = router().await;
    let (status, _, body) = get(&app, "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("asaled"));
}
