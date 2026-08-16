//! "This build is too old to trade" — remembered process-wide, so the UI can
//! say so wherever the user happens to look.
//!
//! The platform refuses an outdated client on the two trading paths, and the
//! two are reached by very different code: the publisher's WS upgrade is
//! rejected before the socket exists, and a buy is refused per request inside
//! the local proxy. Neither of those places can draw anything. Meanwhile the
//! window that *can* draw is idle — it is not the thing being refused.
//!
//! So the refusal is recorded here rather than plumbed through two call chains
//! that have no reason to know about each other, and the desktop shell reads it
//! on the status poll it already makes. Same shape as [`crate::http`]'s proxy
//! preference: one small piece of state several subsystems agree on.
//!
//! Sticky on purpose. A seller whose publisher was refused an hour ago is still
//! running a build that cannot sell, and clearing the banner because nothing has
//! tried since would be telling them the problem went away.

use std::sync::RwLock;

/// What the platform said when it turned this build away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeNotice {
    /// The version this build reports.
    pub current: String,
    /// The oldest version the platform will trade with.
    pub min: String,
    /// Which path was refused, for the UI's wording: `"sell"`, `"buy"`, or
    /// [`PATH_PLATFORM`] when nothing was refused yet and the floor was simply
    /// read.
    pub path: &'static str,
}

/// The floor was learned by asking rather than by being turned away.
///
/// Worth its own value because it is the only one that can appear *before* the
/// user has tried anything: the wording it selects has to explain what is about
/// to stop working, not what just did.
pub const PATH_PLATFORM: &str = "platform";

static NOTICE: RwLock<Option<UpgradeNotice>> = RwLock::new(None);

/// Record a refusal. Later calls overwrite: the newest refusal is the one whose
/// `min` is current, and an operator may raise the floor twice in a day.
pub fn record(min: &str, path: &'static str) {
    let notice = UpgradeNotice {
        current: crate::http::VERSION.to_string(),
        min: min.trim().to_string(),
        path,
    };
    let mut slot = NOTICE.write().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref() != Some(&notice) {
        tracing::warn!(current = %notice.current, min = %notice.min, path, "the platform refused this build as outdated");
    }
    *slot = Some(notice);
}

/// The outstanding refusal, if any.
pub fn get() -> Option<UpgradeNotice> {
    NOTICE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// `(major, minor, patch)`, or `None` for anything this cannot read.
///
/// Deliberately the same rules the server's gate uses (`client_version::parse`
/// there): a two-segment version fills in zeros, a leading `v` is ignored, and a
/// prerelease suffix is dropped rather than ordered — a tester running
/// `0.4.0-rc.1` against a floor of `0.4.0` is running the build the floor was
/// raised *for*, and must not be the first person it blocks.
fn parse(v: &str) -> Option<(u32, u32, u32)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or("");
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Is `current` below `min`?
///
/// False whenever the answer is not clearly yes — an empty floor, a version
/// neither side can parse. Same reasoning as on the server, with more at stake
/// here: this decides whether a modal takes the window over, and a formatting
/// change that made it fire on everything would lock every user out of an app
/// that was working fine.
pub fn is_outdated(current: &str, min: &str) -> bool {
    let (Some(cur), Some(floor)) = (parse(current), parse(min)) else {
        return false;
    };
    cur < floor
}

/// Apply a floor read from the platform: record it when this build is below it,
/// forget any outstanding notice when it is not.
///
/// Only ever called with a floor the server actually answered — a failed poll
/// must not reach here, or an offline minute would read as "the gate is off".
///
/// The clearing half is what makes the poll safe to repeat, and it clears a
/// trade path's refusal too rather than only its own. An operator can lower a
/// floor as easily as raise one, and the alternative is a modal that stays in
/// front of a user whose build became acceptable an hour ago — with no way out
/// of it, since the modal is what stops them making the trade that would prove
/// the platform is happy again. A refusal that is still real comes back within
/// seconds anyway: the publisher reconnects on a loop and the proxy re-records
/// on the next request.
pub fn apply_floor(min: &str) {
    if is_outdated(crate::http::VERSION, min) {
        record(min, PATH_PLATFORM);
    } else {
        clear();
    }
}

/// Forget it — after a successful trade on the path that was refused, which is
/// the only proof that the build is acceptable again (an operator can lower the
/// floor as easily as raise it, and an in-place update keeps the process alive).
pub fn clear() {
    let mut slot = NOTICE.write().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        tracing::info!("the platform is trading with this build again");
    }
    *slot = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_is_remembered_until_a_trade_succeeds() {
        clear();
        assert!(get().is_none());
        record("9.9.9", "sell");
        let n = get().expect("recorded");
        assert_eq!(n.min, "9.9.9");
        assert_eq!(n.path, "sell");
        assert_eq!(n.current, crate::http::VERSION);
        // Still there: nothing has proved the build is acceptable again.
        assert!(get().is_some());
        clear();
        assert!(get().is_none());
    }

    #[test]
    fn ordering_is_numeric_and_forgiving_of_shapes() {
        // 0.10.0 sorts *before* 0.9.0 as a string: the whole reason this is not
        // a `<` on two `&str`.
        assert!(!is_outdated("0.10.0", "0.9.0"));
        assert!(is_outdated("0.3.7", "0.3.8"));
        assert!(!is_outdated("0.3.8", "0.3.8"));
        assert!(!is_outdated("0.4.0-rc.1", "0.4.0"));
        assert_eq!(parse("v1.2"), Some((1, 2, 0)));
        // Nothing usable on either side is never a reason to lock the window.
        assert!(!is_outdated("0.3.7", ""));
        assert!(!is_outdated("0.3.7", "latest"));
        assert!(!is_outdated("", "0.3.8"));
    }

    #[test]
    fn a_floor_this_build_meets_takes_the_dialog_away() {
        clear();
        // Far ahead of anything this build could be.
        apply_floor("999.0.0");
        let n = get().expect("blocked");
        assert_eq!(n.path, PATH_PLATFORM);
        assert_eq!(n.min, "999.0.0");
        // The operator lowered it again.
        apply_floor("0.0.1");
        assert!(get().is_none());
        // And an empty floor is "gate off", not "unknown".
        apply_floor("999.0.0");
        apply_floor("");
        assert!(get().is_none());
        clear();
    }

    #[test]
    fn a_withdrawn_floor_releases_a_trade_paths_refusal_too() {
        // Otherwise the modal outlives the rule it is enforcing: it is what
        // stops the user making the trade that would clear it.
        clear();
        record("999.0.0", "sell");
        apply_floor("0.0.1");
        assert!(get().is_none());
        clear();
    }

    #[test]
    fn the_newest_floor_wins() {
        clear();
        record("0.3.0", "buy");
        record("0.4.0", "sell");
        let n = get().unwrap();
        assert_eq!(n.min, "0.4.0");
        assert_eq!(n.path, "sell");
        clear();
    }
}
