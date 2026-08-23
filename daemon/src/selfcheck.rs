//! Startup health check: the handful of environment faults that make this
//! client look broken in ways its own screens cannot explain.
//!
//! Every check here earned its place by costing someone real time. They share a
//! shape: the app is running, nothing errors visibly, and the symptom the user
//! reports ("my accounts disappeared", "I'm selling but earn nothing") is a
//! consequence several layers away from its cause.
//!
//! **On repairing automatically.** None of these is ours to apply *unasked*:
//! moving a credential store relocates the user's secrets, and flipping the sell
//! switch starts spending their subscription quota. So the diagnosis is
//! automatic and the repair is one button — [`fix`] — which is consent, not
//! silence. A tool that rearranges credentials on its own to be helpful is a
//! worse tool.
//!
//! What that button does has to be the *whole* fix, though. Sending the user to
//! the page where the setting lives is where people give up: a blocked lane is
//! fixed by finding which port the proxy on this machine is listening on, and
//! nobody outside the top decile of users knows that about their own laptop. So
//! `regionBlocked` probes for it and `sellAllOff` throws the switches; a finding
//! whose repair cannot be performed from in here says so by leaving
//! [`Finding::fixable`] false rather than by offering a button that navigates.

use serde::Serialize;
use serde_json::{json, Value};

/// One thing found wrong, in a form the UI can render without knowing what it
/// means: `id` selects the sentence, `params` fills its blanks.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Finding {
    /// Stable key; the UI looks up `selfcheck.<id>.{title,body}`.
    pub id: &'static str,
    /// `error` — nothing works until it is fixed. `warn` — works, earns or
    /// costs less than it should. `info` — worth knowing.
    pub severity: &'static str,
    pub params: Value,
    /// Can [`fix`] resolve this from here? False for the faults whose repair
    /// lives outside the app — an environment variable, how the app is
    /// launched — where the honest answer is the sentence, not a button.
    pub fixable: bool,
}

/// Where this process keeps its state, and where the *other* copy is if the
/// environment has produced two.
///
/// Split state is the fault behind "no token in the secret store" on an account
/// that plainly has one: on Windows `HOME` is set only by unix-ish toolchains,
/// so a daemon started from git-bash and one started from Explorer resolve
/// different homes and get a `.asale` each — same installation, two half-states,
/// neither of them wrong from where it is standing.
///
/// Pure so it can be tested against every environment combination without one.
pub fn check_state_dir(
    data_dir_override: Option<&str>,
    home: Option<&str>,
    userprofile: Option<&str>,
    exists: &dyn Fn(&str) -> bool,
) -> Option<Finding> {
    // An explicit override is a deliberate choice; it is not this check's job to
    // second-guess where an operator pointed the daemon.
    if data_dir_override.is_some_and(|d| !d.trim().is_empty()) {
        return None;
    }
    let (home, userprofile) = (
        home.map(str::trim).filter(|s| !s.is_empty()),
        userprofile.map(str::trim).filter(|s| !s.is_empty()),
    );
    // No *spelling* of home at all: state lands beside the working directory, so
    // it moves whenever the daemon is started from somewhere else. Always wrong.
    //
    // Which spelling is missing does not matter — [`crate::state::home_dir`]
    // accepts any of them, and on Windows launched from Explorer or the Start
    // menu `HOME` is *always* unset while `USERPROFILE` is what resolves. Reading
    // `HOME` alone reported every one of those launches as lost credentials.
    let Some(home) = home.or(userprofile) else {
        return Some(Finding {
            id: "noHome",
            severity: "error",
            params: json!({ "dir": ".asale" }),
            // Setting HOME for a process that is already running would move the
            // state dir out from under this daemon and change nothing about the
            // next launch, which is the one that matters.
            fixable: false,
        });
    };
    // Two homes, both already holding state: whichever this process picked, the
    // other half is invisible to it.
    if let Some(profile) = userprofile {
        let (a, b) = (format!("{home}/.asale"), format!("{profile}/.asale"));
        if !same_path(home, profile) && exists(&a) && exists(&b) {
            return Some(Finding {
                id: "splitState",
                severity: "error",
                params: json!({ "active": a, "other": b }),
                // Merging two credential stores means deciding which copy of a
                // clashing account wins, and getting that wrong loses a
                // subscription's refresh token. Pinning `ASALE_DATA_DIR` is the
                // real fix and it belongs to whatever launches the app.
                fixable: false,
            });
        }
    }
    None
}

/// Windows paths differ in separator and case without differing in meaning.
fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_ascii_lowercase();
    norm(a) == norm(b)
}

/// Is this upstream failure the region block that a proxy fixes?
///
/// From a blocked region the vendor answers *every* request with the same
/// `403 … Request not allowed` — including unauthenticated ones — so it reads as
/// a credential problem and sends people to re-login, which cannot help.
pub fn is_region_block(last_error: &str) -> bool {
    let e = last_error.to_ascii_lowercase();
    e.contains("request not allowed") || (e.contains("403") && e.contains("forbidden"))
}

/// Findings from the lane states the publisher pool is holding.
pub fn check_lanes<'a>(lane_errors: impl Iterator<Item = &'a str>) -> Option<Finding> {
    let blocked = lane_errors.filter(|e| is_region_block(e)).count();
    (blocked > 0).then(|| Finding {
        id: "regionBlocked",
        severity: "warn",
        params: json!({ "lanes": blocked }),
        fixable: true,
    })
}

/// Connected accounts that will never be offered, because selling is off for
/// every one of them.
///
/// Worth saying out loud because signing in again silently resets the switch:
/// the user connected an account, turned selling on, re-authenticated weeks
/// later for an unrelated reason, and has been earning nothing since without
/// touching anything.
pub fn check_sell_switches(total: usize, enabled: usize) -> Option<Finding> {
    (total > 0 && enabled == 0).then(|| Finding {
        id: "sellAllOff",
        severity: "warn",
        params: json!({ "accounts": total }),
        fixable: true,
    })
}

/// Run every check against the live environment.
///
/// Cheap and side-effect free — no network, no writes — so the window can call
/// it on every launch and after the user acts on a finding, without needing to
/// know which of those is which.
pub async fn run(state: &crate::state::AppState) -> Vec<Finding> {
    let mut out = Vec::new();

    // The profile spelling, including the split-across-two-variables form that
    // `home_dir()` falls back to. Same directory either way, so it is the same
    // spelling as far as this check is concerned.
    let userprofile = std::env::var("USERPROFILE").ok().filter(|s| !s.trim().is_empty()).or_else(|| {
        match (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
            (Ok(d), Ok(p)) if !d.trim().is_empty() && !p.trim().is_empty() => Some(format!("{d}{p}")),
            _ => None,
        }
    });
    if let Some(f) = check_state_dir(
        std::env::var("ASALE_DATA_DIR").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        userprofile.as_deref(),
        // "Has this directory ever been used?" — the secret store is the file
        // that only exists once something was actually saved there, which is
        // what distinguishes a real second half from an empty directory.
        &|dir| std::path::Path::new(dir).join("secrets.enc").exists(),
    ) {
        out.push(f);
    }

    crate::publisher::rebuild_pool(&state.store, &state.pool).await;
    let views = match state.pool.lock() {
        Ok(pool) => pool.lane_views(now_secs()),
        // A poisoned lock is a bug worth surfacing elsewhere, but it must not
        // take the checks that did work down with it.
        Err(_) => return out,
    };
    if let Some(f) = check_lanes(views.iter().map(|v| v.last_error.as_str())) {
        out.push(f);
    }
    // Per account, not per lane: one subscription sells many models, and
    // "3 accounts, none selling" is the sentence a user can act on.
    let mut accounts = std::collections::HashMap::<&str, bool>::new();
    for v in &views {
        let e = accounts.entry(v.account_id.as_str()).or_insert(false);
        *e |= v.sell_enabled;
    }
    if let Some(f) = check_sell_switches(accounts.len(), accounts.values().filter(|on| **on).count()) {
        out.push(f);
    }
    out
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── repairs ────────────────────────────────────────────────────────────────

/// Loopback ports a proxy on this machine is plausibly listening on.
///
/// This list is the whole reason `regionBlocked` can be fixed by a button
/// instead of by a support conversation: the user knows they "have a VPN on",
/// and does not know it exposes an HTTP proxy, let alone on which port. These
/// are the defaults the common clients ship with — Clash and its forks (7890,
/// and 7897 since Verge), v2rayN (10809), the v2ray/shadowsocks convention
/// (1087), Privoxy (8118), Surge on macOS (6152). A port that answers but is not
/// a proxy simply fails the probe below, so a wrong guess costs one request.
const PROXY_PORTS: &[u16] = &[7890, 7897, 10809, 1087, 8118, 6152];

/// Apply the repair for `id`, or explain that there is not one.
///
/// Returns `{fixed, key, params}` — a translation key rather than a sentence,
/// like every other command, so the four locales stay in the frontend catalog.
/// `fixed: false` is a normal outcome, not an error: "we looked for a proxy on
/// this machine and there is none" is an answer the user needs, and raising it
/// as a failure would render it as a red toast with a stack trace in it.
pub async fn fix(state: &crate::state::AppState, id: &str) -> crate::commands::R<Value> {
    match id {
        "sellAllOff" => fix_sell_all_off(state).await,
        "regionBlocked" => fix_region_blocked(state).await,
        other => Err(crate::cmd_err!(
            "errors.selfcheck.notFixable",
            format!("no automatic repair for `{other}`"),
            id = other
        )),
    }
}

/// Put every connected account back on the market.
///
/// All of them, not the first: the finding fires only when *none* is selling, so
/// there is no per-account intent here to preserve — the state it repairs is the
/// one a re-login leaves behind, which switched off accounts the user had
/// already chosen to sell.
async fn fix_sell_all_off(state: &crate::state::AppState) -> crate::commands::R<Value> {
    let tools = state.store.list_tools().await.map_err(crate::commands::err)?;
    let mut turned_on = 0usize;
    for t in tools.iter().filter(|t| !t.sell_enabled) {
        // Propagated, not swallowed: the first failure here is almost always
        // "sign in before selling", and that is exactly what the user has to be
        // told. Accounts already switched on stay on — this is not a rollback.
        crate::commands::set_account_sell(
            state,
            t.provider.clone(),
            t.account_id.clone(),
            true,
            None,
            None,
            None,
            None,
            None,
        )
        .await?;
        turned_on += 1;
    }
    Ok(json!({
        "fixed": turned_on > 0,
        "key": "selfcheck.sellAllOff.fixed",
        "params": { "accounts": turned_on },
    }))
}

/// Find the proxy this machine already runs and route provider calls through it.
///
/// Two phases on purpose. A TCP connect answers in milliseconds and rules out
/// every port with nothing behind it, so the expensive part — an actual request
/// to a provider endpoint, the only thing that proves the block is lifted — runs
/// against the one or two candidates that could possibly work. Probing all six
/// the slow way would take a minute of a user staring at a spinner.
async fn fix_region_blocked(state: &crate::state::AppState) -> crate::commands::R<Value> {
    let mut candidates: Vec<(&'static str, String)> = Vec::new();
    // Whatever the environment or the OS already says, first: if that works, the
    // fault was only that the saved preference had it switched off, and `auto`
    // is a better thing to leave behind than a hard-coded port.
    if let Some(url) = asale_client_core::http::auto_proxy() {
        candidates.push(("auto", url));
    }
    for port in PROXY_PORTS {
        let url = format!("http://127.0.0.1:{port}");
        // A proxy the environment already named does not need probing twice.
        if candidates.iter().any(|(_, u)| u == &url) {
            continue;
        }
        if listening(*port).await {
            candidates.push(("manual", url));
        }
    }

    let mut tried = Vec::new();
    for (mode, url) in &candidates {
        tried.push(url.clone());
        let probe = crate::commands::test_proxy(mode.to_string(), url.clone()).await?;
        if probe["ok"].as_bool().unwrap_or(false) {
            crate::commands::set_proxy_settings(state, mode.to_string(), url.clone()).await?;
            tracing::info!(%url, mode, "self-check repaired a region block by routing through a local proxy");
            return Ok(json!({
                "fixed": true,
                "key": "selfcheck.regionBlocked.fixed",
                "params": { "proxy": url },
            }));
        }
    }
    // Nothing worked. Saying which ports were looked at is what turns this from
    // "it failed" into something the user can carry to their proxy app.
    Ok(json!({
        "fixed": false,
        "key": if tried.is_empty() { "selfcheck.regionBlocked.fixNoneFound" } else { "selfcheck.regionBlocked.fixNoneWorked" },
        "params": { "tried": tried.join(", "), "count": tried.len() },
    }))
}

/// Is anything accepting connections on `port` of this machine?
///
/// Short timeout by design: a local port either answers immediately or is not
/// there, and six of these run before the user's first spinner frame.
async fn listening(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_millis(300),
            tokio::net::TcpStream::connect(addr),
        )
        .await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> bool {
        false
    }
    fn all(_: &str) -> bool {
        true
    }

    #[test]
    fn no_home_is_always_a_fault() {
        let f = check_state_dir(None, None, None, &none).expect("flagged");
        assert_eq!(f.id, "noHome");
        assert_eq!(f.severity, "error");
    }

    /// Windows launched from Explorer or the Start menu: `HOME` unset,
    /// `USERPROFILE` set. `data_dir()` resolves it, so there is nothing wrong —
    /// and this is the *default* way the app is started, so getting it wrong
    /// showed every Windows user a red "your credentials are lost" banner.
    #[test]
    fn the_profile_variable_alone_is_a_perfectly_good_home() {
        assert!(check_state_dir(None, None, Some("C:/Users/u"), &all).is_none());
        assert!(check_state_dir(None, None, Some("C:/Users/u"), &none).is_none());
        // Blank counts as unset, here as everywhere else.
        assert_eq!(
            check_state_dir(None, Some("  "), Some("C:/Users/u"), &all).is_none(),
            true
        );
    }

    #[test]
    fn an_explicit_data_dir_is_never_second_guessed() {
        assert!(check_state_dir(Some("D:/asale"), None, None, &none).is_none());
        // Empty is not a choice, it is an unset variable read badly.
        assert!(check_state_dir(Some("  "), None, None, &none).is_some());
    }

    #[test]
    fn two_homes_are_only_a_split_when_both_hold_state() {
        // The git-bash / Explorer case: two homes, state under each.
        let f = check_state_dir(None, Some("C:/msys/home/u"), Some("C:/Users/u"), &all).expect("flagged");
        assert_eq!(f.id, "splitState");
        // Two homes but only one has ever been used: nothing is split yet.
        assert!(check_state_dir(None, Some("C:/msys/home/u"), Some("C:/Users/u"), &none).is_none());
    }

    #[test]
    fn the_same_home_spelled_two_ways_is_not_a_split() {
        // `HOME` and `USERPROFILE` pointing at one directory is the *normal*
        // Windows setup once git-bash has run; flagging it would make the
        // warning permanent and therefore ignored.
        assert!(check_state_dir(None, Some("C:\\Users\\u"), Some("C:/Users/u/"), &all).is_none());
    }

    #[test]
    fn a_region_block_is_told_apart_from_an_ordinary_auth_failure() {
        assert!(is_region_block("forbidden: Request not allowed"));
        assert!(is_region_block("upstream 403 forbidden"));
        // The one that must not match: a real credential problem, whose fix is
        // to sign in again rather than to configure a proxy.
        assert!(!is_region_block("401 unauthorized: invalid api key"));
        assert!(!is_region_block("429 rate limited"));
    }

    #[test]
    fn lanes_are_only_flagged_when_one_is_actually_blocked() {
        assert!(check_lanes(["401 unauthorized", ""].into_iter()).is_none());
        let f = check_lanes(["forbidden: Request not allowed", "401"].into_iter()).expect("flagged");
        assert_eq!(f.params["lanes"], 1);
    }

    #[test]
    fn selling_is_only_flagged_when_every_account_is_off() {
        assert!(check_sell_switches(0, 0).is_none(), "no accounts is not a fault");
        assert!(check_sell_switches(3, 1).is_none(), "one is earning; the others may be deliberate");
        assert_eq!(check_sell_switches(3, 0).unwrap().id, "sellAllOff");
    }

    /// A finding claims a button only when pressing it does the whole job. The
    /// failure this guards against is the one the button rework existed to
    /// remove: an offer to fix something that then hands the user back the same
    /// question somewhere else.
    #[test]
    fn only_the_findings_this_process_can_actually_repair_offer_a_button() {
        assert!(check_lanes(["forbidden: Request not allowed"].into_iter()).unwrap().fixable);
        assert!(check_sell_switches(3, 0).unwrap().fixable);
        // Both of these are repaired by changing how the app is launched, which
        // a running process cannot do to itself.
        assert!(!check_state_dir(None, None, None, &none).unwrap().fixable);
        assert!(!check_state_dir(None, Some("C:/msys/home/u"), Some("C:/Users/u"), &all).unwrap().fixable);
    }

    /// The candidate list is the whole value of the region-block repair, so it
    /// has to stay a list of *proxy* defaults. A port that is merely common —
    /// 8080, 3000 — would send a probe at whatever dev server the user has
    /// running and, on a machine behind a transparent proxy, could even succeed
    /// and route provider traffic through it.
    #[test]
    fn the_probed_ports_are_proxy_defaults_and_nothing_else() {
        assert!(PROXY_PORTS.contains(&7890), "clash");
        assert!(PROXY_PORTS.contains(&10809), "v2rayN");
        for p in [8080u16, 3000, 5173, 9700] {
            assert!(!PROXY_PORTS.contains(&p), "{p} is not a proxy default");
        }
        let mut sorted = PROXY_PORTS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), PROXY_PORTS.len(), "a duplicate would probe the same port twice");
    }
}
