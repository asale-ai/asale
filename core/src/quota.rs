//! Turning a provider's *own* rate-limit windows into a sell-side gate.
//!
//! This used to be one of two answers. The other one — a guessed plan cap minus
//! what this device had sold — is gone (see
//! [`crate::discovery::plan_window_cap`]): it counted only asale's own sales, so
//! the operator's own Claude Code session was invisible to it, and its cap fell
//! back to the lowest paid tier whenever the login carried no plan, which for
//! Claude's OAuth exchange is always. It took live subscriptions off the market
//! for windows it had invented.
//!
//! What is left here reads only what the provider itself publishes: Claude
//! answers `oauth/usage` with a utilisation percentage and a reset instant per
//! window, and Codex rides the same figures back on `x-codex-*` response
//! headers. Both are normalised into one shape (`{key, label, used_percent,
//! reset_at, window_seconds}`) by `commands::usage`, banked in the local store,
//! and read back here.
//!
//! # What this gate may and may not do
//!
//! It may take a lane off the market for a **model-scoped** window the vendor
//! says is spent (below), and it may name the instant a spent window returns.
//! It does **not** turn a percentage into a token headroom any more: no local
//! arithmetic decides that a subscription is finished. That verdict belongs to
//! the upstream, and it delivers it as a 429 —
//! `pool::AccountPool::on_error` cools the account until the reset the upstream
//! itself names.
//!
//! # Model-scoped windows
//!
//! Anthropic meters Opus (and other named models) on their own weekly window on
//! top of the account-wide ones. Spending it must take Opus off the market and
//! leave Sonnet selling — so those windows are kept apart from the account-wide
//! ones and applied per lane.

use serde_json::Value;

/// A model-scoped window that is spent, and when it comes back.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeBlock {
    /// The scope's own name, lowercased — `opus`, `fable`. Matched against
    /// model ids by [`scope_matches_model`].
    pub scope: String,
    /// When the window resets, if the provider said.
    pub reset_at: Option<i64>,
}

/// What a provider's own rate-limit windows allow this account to sell.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaGate {
    /// Fraction of the subscription still serviceable (0.0..=1.0), taken from
    /// the tightest window that applies to every model.
    pub headroom: f64,
    /// When that tightest window resets. `None` while nothing is spent, or when
    /// the provider reported no instant.
    pub reset_at: Option<i64>,
    /// Windows scoped to one model family that are spent.
    pub blocked: Vec<ScopeBlock>,
    /// The key of the window `headroom` came from, for the log line that
    /// explains a lane leaving the market.
    pub tightest: String,
}

impl QuotaGate {
    /// Nothing left to sell account-wide.
    pub fn exhausted(&self) -> bool {
        self.headroom <= 0.0
    }

    /// Whether one model is blocked by a scoped window (Opus's own weekly cap),
    /// and when it returns.
    pub fn scope_block(&self, model: &str) -> Option<&ScopeBlock> {
        self.blocked.iter().find(|b| scope_matches_model(&b.scope, model))
    }
}

/// Keys of the windows that meter the whole subscription rather than one model.
///
/// Claude's are `5h` and `7d`; Codex's are named after their duration by
/// `normalize_codex_headers`, which produces the same two strings for the same
/// two windows. Anything else — `7d_opus`, `ws_Fable` — is model-scoped and
/// handled as a scope block.
fn is_account_wide(key: &str) -> bool {
    !key.starts_with("ws_") && !key.contains('_')
}

/// The scope name a model-scoped window key carries: `7d_opus` → `opus`,
/// `ws_Fable` → `fable`.
fn scope_of(key: &str) -> Option<String> {
    let name = key.strip_prefix("ws_").or_else(|| key.rsplit_once('_').map(|(_, s)| s))?;
    let name = name.trim().to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

/// Whether a scoped window covers a model id.
///
/// Substring rather than an exact list: the scope arrives as the vendor's own
/// display name (`Opus`, `Fable`) and the market ids are `claude-opus-5`,
/// `claude-fable-5`. A names table would need editing every time a vendor ships
/// a model, and the failure mode of missing an entry is selling a lane whose
/// upstream will refuse it.
pub fn scope_matches_model(scope: &str, model: &str) -> bool {
    !scope.is_empty() && model.to_ascii_lowercase().contains(scope)
}

/// Read a window's `reset_at`, whichever way the provider spelled it.
///
/// Claude sends RFC 3339 (`2026-08-17T07:19:59.690710+00:00`), Codex sends unix
/// seconds, and the local estimate sends unix seconds too. There is no date
/// crate in this dependency tree, and this is the one field that needs one.
pub fn parse_reset_at(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().filter(|t| *t > 0),
        Value::String(s) => parse_rfc3339(s),
        _ => None,
    }
}

/// `2026-08-17T07:19:59.690710+00:00` → unix seconds. Fractional seconds are
/// dropped; an explicit offset is honoured.
fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse().ok() };
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hh, mm, ss) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let secs = days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss;
    // Trailing zone: `Z`, `+08:00`, `-05:00`, or nothing (read as UTC).
    let rest = &s[19..];
    let zone = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match zone.as_bytes().first() {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let hh: i64 = zone.get(1..3)?.parse().ok()?;
            let mm: i64 = zone.get(4..6).unwrap_or("00").parse().unwrap_or(0);
            let mag = hh * 3600 + mm * 60;
            if *sign == b'+' {
                mag
            } else {
                -mag
            }
        }
        _ => return None,
    };
    Some(secs - offset)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's `days_from_civil`),
/// the inverse of the `civil_from_days` in `cli_import::rfc3339_utc`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Build the gate from a provider's normalised windows.
///
/// `None` when the reading carries no window that can decide anything — an
/// empty array, or one whose entries have no `used_percent`. The caller falls
/// back to the local estimate there rather than reading "no windows" as "no
/// limits", which would over-declare supply on a spent account.
///
/// A window whose reset instant has already passed is skipped: the reading is
/// describing a window that has since rolled over, so its utilisation is
/// history. That is what keeps a stale snapshot from holding a lane off the
/// market past the very instant it was supposed to come back at.
pub fn gate_from_windows(windows: &[Value], now: i64) -> Option<QuotaGate> {
    let mut headroom: Option<(f64, String, Option<i64>)> = None;
    let mut blocked = Vec::new();
    for w in windows {
        let Some(key) = w.get("key").and_then(Value::as_str) else { continue };
        let Some(pct) = w.get("used_percent").and_then(Value::as_f64) else { continue };
        let reset_at = w.get("reset_at").and_then(parse_reset_at);
        if reset_at.is_some_and(|t| t <= now) {
            continue;
        }
        let free = (1.0 - pct / 100.0).clamp(0.0, 1.0);
        if is_account_wide(key) {
            if headroom.as_ref().is_none_or(|(prev, _, _)| free < *prev) {
                headroom = Some((free, key.to_string(), reset_at));
            }
        } else if free <= 0.0 {
            if let Some(scope) = scope_of(key) {
                blocked.push(ScopeBlock { scope, reset_at });
            }
        }
    }
    let (headroom, tightest, reset_at) = headroom?;
    Some(QuotaGate {
        headroom,
        // Only meaningful while something is actually spent: a window at 40%
        // has a reset instant too, and reporting it would put a countdown on a
        // lane that is selling.
        reset_at: (headroom <= 0.0).then_some(reset_at).flatten(),
        blocked,
        tightest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_786_951_459; // 2026-08-17T07:24:19Z

    fn claude_windows(five_h: f64, seven_d: f64) -> Vec<Value> {
        vec![
            json!({"key":"5h","label":"5h","used_percent":five_h,"reset_at":"2026-08-17T09:19:59.690710+00:00","window_seconds":18000}),
            json!({"key":"7d","label":"7d","used_percent":seven_d,"reset_at":"2026-08-20T09:59:59.690726+00:00","window_seconds":604800}),
        ]
    }

    /// The production failure this module exists for: Anthropic says the 5h
    /// window is 3% spent while the local estimate — a guessed 220k cap against
    /// 239k of local sales — says the account is finished.
    #[test]
    fn the_upstreams_own_number_keeps_a_live_subscription_selling() {
        let g = gate_from_windows(&claude_windows(3.0, 48.0), NOW).unwrap();
        assert!(!g.exhausted());
        // The weekly window is the binding one at 48% spent, not the 5h at 3%.
        assert!((g.headroom - 0.52).abs() < 1e-9);
        assert_eq!(g.tightest, "7d");
        // Nothing is spent, so there is no countdown to show.
        assert_eq!(g.reset_at, None);
    }

    #[test]
    fn the_tightest_account_wide_window_decides() {
        let g = gate_from_windows(&claude_windows(20.0, 95.0), NOW).unwrap();
        assert!((g.headroom - 0.05).abs() < 1e-9);
        assert_eq!(g.tightest, "7d");
    }

    #[test]
    fn a_spent_window_carries_the_instant_it_comes_back() {
        let g = gate_from_windows(&claude_windows(100.0, 48.0), NOW).unwrap();
        assert!(g.exhausted());
        // 2026-08-17T09:19:59Z, parsed out of the RFC 3339 the vendor sent.
        assert_eq!(g.reset_at, Some(1_786_958_399));
    }

    /// Opus has its own weekly window. Spending it must take Opus off the
    /// market and leave every other model of the same subscription selling.
    #[test]
    fn a_model_scoped_window_blocks_only_its_own_models() {
        let mut w = claude_windows(10.0, 40.0);
        w.push(json!({"key":"7d_opus","label":"7d Opus","used_percent":100.0,
                      "reset_at":"2026-08-20T09:59:59+00:00","window_seconds":604800}));
        w.push(json!({"key":"ws_Fable","label":"7d Fable","used_percent":18.0,
                      "reset_at":"2026-08-20T09:59:59+00:00","window_seconds":604800}));
        let g = gate_from_windows(&w, NOW).unwrap();
        assert!(!g.exhausted(), "the account itself is fine");
        assert_eq!(g.blocked.len(), 1);
        assert!(g.scope_block("claude-opus-5").is_some());
        assert!(g.scope_block("claude-opus-4-6").is_some());
        assert!(g.scope_block("claude-sonnet-5").is_none());
        // A scoped window with headroom left is not a block.
        assert!(g.scope_block("claude-fable-5").is_none());
    }

    /// A banked reading describes the window it was taken in. Once that window
    /// has rolled over, its utilisation is history — and holding a lane off the
    /// market on it would outlast the very reset the provider announced.
    #[test]
    fn a_window_whose_reset_has_passed_is_not_read_as_still_spent() {
        let stale = vec![json!({"key":"5h","used_percent":100.0,"reset_at":NOW - 60,"window_seconds":18000})];
        assert_eq!(gate_from_windows(&stale, NOW), None, "nothing left to decide on");
        let mut mixed = stale.clone();
        mixed.push(json!({"key":"7d","used_percent":20.0,"reset_at":NOW + 86_400,"window_seconds":604_800}));
        let g = gate_from_windows(&mixed, NOW).unwrap();
        assert!(!g.exhausted(), "only the live window counts");
        assert_eq!(g.tightest, "7d");
    }

    #[test]
    fn no_readable_window_is_not_an_unlimited_account() {
        assert_eq!(gate_from_windows(&[], NOW), None);
        assert_eq!(gate_from_windows(&[json!({"key":"5h"})], NOW), None);
    }

    /// Codex spells its windows by duration and its resets as unix seconds.
    #[test]
    fn codex_windows_read_the_same_way() {
        let w = vec![
            json!({"key":"5h","label":"5h","used_percent":43.0,"reset_at":NOW + 3600,"window_seconds":18000}),
            json!({"key":"7d","label":"7d","used_percent":5.0,"reset_at":NOW + 400_000,"window_seconds":604_800}),
        ];
        let g = gate_from_windows(&w, NOW).unwrap();
        assert!((g.headroom - 0.57).abs() < 1e-9);
        assert_eq!(g.tightest, "5h");
    }

    #[test]
    fn rfc3339_parses_the_shapes_the_vendors_send() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-08-17T07:19:59.690710+00:00"), Some(1_786_951_199));
        assert_eq!(parse_rfc3339("2026-08-17T07:19:59Z"), Some(1_786_951_199));
        // An offset is honoured rather than ignored.
        assert_eq!(parse_rfc3339("2026-08-17T15:19:59+08:00"), Some(1_786_951_199));
        assert_eq!(parse_rfc3339("2026-08-17T02:19:59-05:00"), Some(1_786_951_199));
        assert_eq!(parse_rfc3339("not a date"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
        // Numbers pass through as the unix seconds they already are.
        assert_eq!(parse_reset_at(&json!(1_786_951_199_i64)), Some(1_786_951_199));
        assert_eq!(parse_reset_at(&json!(0)), None);
        assert_eq!(parse_reset_at(&Value::Null), None);
    }
}
