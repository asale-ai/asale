//! Reconciliation (spec §8.1). Compares the local task records against the
//! server's authoritative records by `task_id` and summarizes the differences.
//! The server is authoritative — the caller syncs local amounts from matched
//! server rows and surfaces the summary to the user.

use serde::Serialize;
use std::collections::HashMap;

/// One record side (local or server), reduced to the reconciliation keys.
#[derive(Debug, Clone, PartialEq)]
pub struct RecEntry {
    pub task_id: String,
    /// in+out tokens.
    pub tokens: i64,
    /// micro-USDT.
    pub amount: i64,
}

/// Difference summary between local and server record sets.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ReconcileSummary {
    /// task_ids present on both sides.
    pub matched: usize,
    /// Local records the server does not know about.
    pub local_only: usize,
    /// Server records missing locally.
    pub server_only: usize,
    /// Matched pairs whose token totals disagree (both sides non-zero).
    pub token_mismatch: usize,
    /// Matched pairs whose amounts disagree (only when the local side has one).
    pub amount_mismatch: usize,
}

/// Matched pair whose local amount should be updated to the server's value.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AmountFix {
    pub task_id: String,
    pub server_amount: i64,
}

/// Diff local vs server records by task_id.
///
/// Returns the summary plus the amount fixes to apply locally (server wins,
/// spec §8.1). A local amount of 0 means "not settled locally yet" and is not
/// counted as a mismatch — it is still returned as a fix so the local ledger
/// converges to the server's numbers.
pub fn diff(local: &[RecEntry], server: &[RecEntry]) -> (ReconcileSummary, Vec<AmountFix>) {
    let server_by_id: HashMap<&str, &RecEntry> = server.iter().map(|r| (r.task_id.as_str(), r)).collect();
    let local_ids: std::collections::HashSet<&str> = local.iter().map(|r| r.task_id.as_str()).collect();

    let mut sum = ReconcileSummary::default();
    let mut fixes = Vec::new();

    for l in local {
        match server_by_id.get(l.task_id.as_str()) {
            Some(s) => {
                sum.matched += 1;
                if l.tokens > 0 && s.tokens > 0 && l.tokens != s.tokens {
                    sum.token_mismatch += 1;
                }
                if l.amount != s.amount {
                    if l.amount > 0 {
                        sum.amount_mismatch += 1;
                    }
                    fixes.push(AmountFix { task_id: l.task_id.clone(), server_amount: s.amount });
                }
            }
            None => sum.local_only += 1,
        }
    }
    sum.server_only = server.iter().filter(|s| !local_ids.contains(s.task_id.as_str())).count();
    (sum, fixes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str, tokens: i64, amount: i64) -> RecEntry {
        RecEntry { task_id: id.into(), tokens, amount }
    }

    #[test]
    fn clean_match_produces_no_diffs() {
        let local = vec![e("t1", 100, 5), e("t2", 200, 10)];
        let server = vec![e("t1", 100, 5), e("t2", 200, 10)];
        let (sum, fixes) = diff(&local, &server);
        assert_eq!(sum, ReconcileSummary { matched: 2, ..Default::default() });
        assert!(fixes.is_empty());
    }

    #[test]
    fn counts_local_only_and_server_only() {
        let local = vec![e("t1", 100, 5), e("only-local", 10, 0)];
        let server = vec![e("t1", 100, 5), e("only-server", 20, 3)];
        let (sum, _) = diff(&local, &server);
        assert_eq!(sum.matched, 1);
        assert_eq!(sum.local_only, 1);
        assert_eq!(sum.server_only, 1);
    }

    #[test]
    fn token_and_amount_mismatches() {
        let local = vec![e("t1", 100, 5), e("t2", 50, 7)];
        let server = vec![e("t1", 120, 5), e("t2", 50, 9)];
        let (sum, fixes) = diff(&local, &server);
        assert_eq!(sum.token_mismatch, 1);
        assert_eq!(sum.amount_mismatch, 1);
        assert_eq!(fixes, vec![AmountFix { task_id: "t2".into(), server_amount: 9 }]);
    }

    #[test]
    fn zero_local_amount_is_a_fix_not_a_mismatch() {
        // Local metering has no price → amount 0. The server's settled amount
        // must flow back as a fix without flagging a mismatch.
        let local = vec![e("t1", 100, 0)];
        let server = vec![e("t1", 100, 42)];
        let (sum, fixes) = diff(&local, &server);
        assert_eq!(sum.amount_mismatch, 0);
        assert_eq!(fixes, vec![AmountFix { task_id: "t1".into(), server_amount: 42 }]);
    }

    #[test]
    fn zero_token_sides_do_not_count_as_token_mismatch() {
        // A failed local record (0 tokens) vs server's metered row.
        let local = vec![e("t1", 0, 0)];
        let server = vec![e("t1", 90, 4)];
        let (sum, _) = diff(&local, &server);
        assert_eq!(sum.token_mismatch, 0);
        assert_eq!(sum.matched, 1);
    }
}
