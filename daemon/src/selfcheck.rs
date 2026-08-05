//! Startup health check: the handful of environment faults that make this
//! client look broken in ways its own screens cannot explain.
//!
//! Every check here earned its place by costing someone real time. They share a
//! shape: the app is running, nothing errors visibly, and the symptom the user
//! reports ("my accounts disappeared", "I'm selling but earn nothing") is a
//! consequence several layers away from its cause.
//!
//! **On repairing automatically.** Each of these has an obvious "fix" and none
//! of them is ours to apply unasked: moving a credential store relocates the
//! user's secrets, and flipping the sell switch starts spending their
//! subscription quota. What is automated is the *diagnosis* — the part that
//! actually needs the system's knowledge — plus, where one exists, a single
//! button that performs the fix with the user's consent. A tool that silently
//! rearranges credentials to be helpful is a worse tool.

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
    // No home at all: state lands beside the working directory, so it moves
    // whenever the daemon is started from somewhere else. Always wrong.
    let Some(home) = home else {
        return Some(Finding {
            id: "noHome",
            severity: "error",
            params: json!({ "dir": ".asale" }),
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
    })
}

/// Run every check against the live environment.
///
/// Cheap and side-effect free — no network, no writes — so the window can call
/// it on every launch and after the user acts on a finding, without needing to
/// know which of those is which.
pub async fn run(state: &crate::state::AppState) -> Vec<Finding> {
    let mut out = Vec::new();

    if let Some(f) = check_state_dir(
        std::env::var("ASALE_DATA_DIR").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("USERPROFILE").ok().as_deref(),
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
}
