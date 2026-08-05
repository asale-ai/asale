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
    /// Which path was refused, for the UI's wording: `"sell"` or `"buy"`.
    pub path: &'static str,
}

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
