//! This seller's standing with the matcher, as the gateway last reported it.
//!
//! A seller under the reputation floor is the quietest failure in the system:
//! the client is connected, its lanes are declared and indexed, its own console
//! says "selling", and requests barely arrive. Nothing in that picture is wrong
//! from the client's side, so the client cannot infer it — only the gateway
//! knows the score and the floor it compares against.
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
    /// The floor matching applies. Below it, every lane at or above the floor is
    /// served ahead of this one.
    pub min_score: i32,
}

impl SellerStatus {
    /// Is this seller at the back of the queue for every model it sells?
    ///
    /// Not *excluded* — the gateway would rather serve a weak lane than refuse a
    /// buyer, so a model with no healthier supply still routes here. But on any
    /// model that has one, this seller is last in line and will see close to
    /// nothing, which is what the publish page has to explain.
    ///
    /// A zero floor means the deployment does not rank on reputation at all, so
    /// no score can be under it — guarded explicitly because a gateway that
    /// reported nothing would otherwise leave both fields at zero and read as
    /// "last in line at 0/0".
    pub fn deprioritised(&self) -> bool {
        self.min_score > 0 && self.score < self.min_score
    }
}

static STATUS: RwLock<Option<SellerStatus>> = RwLock::new(None);

pub fn record(score: i32, min_score: i32) {
    let next = SellerStatus { score, min_score };
    let mut slot = STATUS.write().unwrap_or_else(|e| e.into_inner());
    let was_down = slot.map(|s| s.deprioritised()).unwrap_or(false);
    if next.deprioritised() && !was_down {
        tracing::warn!(
            score,
            min_score,
            "reputation is under the matching floor — healthier lanes will be served first"
        );
    } else if was_down && !next.deprioritised() {
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
    fn under_the_floor_is_last_in_line_and_at_it_is_not() {
        assert!(SellerStatus { score: 526, min_score: 600 }.deprioritised());
        assert!(!SellerStatus { score: 600, min_score: 600 }.deprioritised());
        assert!(!SellerStatus { score: 700, min_score: 600 }.deprioritised());
    }

    #[test]
    fn a_deployment_with_no_floor_never_reports_a_ranking_problem() {
        // Also the shape an older gateway leaves behind: both fields defaulted
        // to zero, which must not render as "last in line at 0/0".
        assert!(!SellerStatus { score: 0, min_score: 0 }.deprioritised());
    }

    #[test]
    fn the_latest_report_replaces_the_last() {
        record(526, 600);
        assert!(get().unwrap().deprioritised());
        record(700, 600);
        assert!(!get().unwrap().deprioritised());
    }
}
