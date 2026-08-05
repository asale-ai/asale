//! This seller's standing with the matcher, as the gateway last reported it.
//!
//! A seller below the reputation floor is the quietest failure in the system:
//! the client is connected, its lanes are declared and indexed, its own console
//! says "selling", and requests simply never arrive. Nothing in that picture is
//! wrong from the client's side, so the client cannot infer it — only the
//! gateway knows the score and the floor it compares against.
//!
//! So the gateway reports both on every supply declaration and this is where the
//! answer is kept, for the publish page to read on its next poll. Same shape and
//! the same reason as [`crate::upgrade`]: a fact learned deep in the publisher
//! session that has to reach a window which is not otherwise involved.

use std::sync::RwLock;

/// Reputation as of the last supply declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SellerStatus {
    pub score: i32,
    /// The floor matching applies. Below it, every lane is filtered out.
    pub min_score: i32,
}

impl SellerStatus {
    /// Is this seller being skipped by matching entirely?
    ///
    /// A zero floor means the deployment does not gate on reputation at all, so
    /// no score can be under it — guarded explicitly because a gateway that
    /// reported nothing would otherwise leave both fields at zero and read as
    /// "blocked at 0/0".
    pub fn blocked(&self) -> bool {
        self.min_score > 0 && self.score < self.min_score
    }
}

static STATUS: RwLock<Option<SellerStatus>> = RwLock::new(None);

pub fn record(score: i32, min_score: i32) {
    let next = SellerStatus { score, min_score };
    let mut slot = STATUS.write().unwrap_or_else(|e| e.into_inner());
    let was_blocked = slot.map(|s| s.blocked()).unwrap_or(false);
    if next.blocked() && !was_blocked {
        tracing::warn!(
            score,
            min_score,
            "reputation is under the matching floor — this device is declared but will not be matched"
        );
    } else if was_blocked && !next.blocked() {
        tracing::info!(score, min_score, "reputation is back above the matching floor");
    }
    *slot = Some(next);
}

/// The last reported standing, or `None` if the gateway has not said — an older
/// gateway, or a session that has not declared supply yet.
pub fn get() -> Option<SellerStatus> {
    *STATUS.read().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_the_floor_is_blocked_and_at_it_is_not() {
        assert!(SellerStatus { score: 526, min_score: 600 }.blocked());
        assert!(!SellerStatus { score: 600, min_score: 600 }.blocked());
        assert!(!SellerStatus { score: 700, min_score: 600 }.blocked());
    }

    #[test]
    fn a_deployment_with_no_floor_never_reports_blocked() {
        // Also the shape an older gateway leaves behind: both fields defaulted
        // to zero, which must not render as "blocked at 0/0".
        assert!(!SellerStatus { score: 0, min_score: 0 }.blocked());
    }

    #[test]
    fn the_latest_report_replaces_the_last() {
        record(526, 600);
        assert!(get().unwrap().blocked());
        record(700, 600);
        assert!(!get().unwrap().blocked());
    }
}
