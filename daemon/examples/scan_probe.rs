//! Run the local-CLI usage scanner against the REAL `~/.asale/asale.db` and
//! print the resulting "我使用的" (used) totals from your real Claude Code logs.
//!
//!   cargo run --example scan_probe

use asale_client_core::store::LocalStore;
use asale_daemon::usage_scan;

#[tokio::main]
async fn main() {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let store = LocalStore::open(&format!("{home}/.asale/asale.db")).await.expect("open store");

    let folded = usage_scan::scan_claude_logs(&store).await.expect("scan");
    println!("scan folded {folded} assistant message(s) from local Claude Code logs.\n");

    let (tokens, _amount, count) = store.agg_totals(&["used"], None).await.unwrap();
    println!("── scope=used: total_tokens={tokens}  messages={count}");
    for (m, tk, c) in store.agg_by_model(&["used"], None, 8).await.unwrap() {
        let share = if tokens > 0 { tk as f64 / tokens as f64 * 100.0 } else { 0.0 };
        println!("     {m:<22} {tk:>12} tok  {share:5.1}%  ({c} msgs)");
    }
    let daily = store.agg_by_day(&["used"], None).await.unwrap();
    let (active, first) = store.agg_active(&["used"]).await.unwrap();
    println!("     days with data: {}  active_days={active}  first_day={first:?}", daily.len());
    if let Some((d, t, ..)) = daily.last() {
        println!("     latest day {d}: {t} tok");
    }
}
