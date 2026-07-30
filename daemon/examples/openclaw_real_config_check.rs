//! Dry-run of the OpenClaw buy switch against this machine's *real*
//! `~/.openclaw/openclaw.json` — the file is copied into a throwaway $HOME
//! first, so the original is never opened for writing.
//!
//! The unit tests cover a config shaped like the real one; this covers the real
//! one, which is the point of having picked a tool that is actually installed.
//!
//!   cargo run --example openclaw_real_config_check

use asale_daemon::tool_config;

const KEY: &str = "sk-asale-DRYRUN";
const MODELS: [&str; 2] = ["claude-fable-5", "gpt-5.6-terra"];

fn main() -> anyhow::Result<()> {
    // Read the real config through whichever variable names the home directory
    // on this platform, before $HOME is pointed elsewhere.
    let real_home = ["HOME", "USERPROFILE"]
        .into_iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()))
        .ok_or_else(|| anyhow::anyhow!("no HOME/USERPROFILE"))?;
    let real = std::path::Path::new(&real_home).join(".openclaw").join("openclaw.json");
    let original = std::fs::read_to_string(&real)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", real.display()))?;
    println!("source: {} ({} bytes)", real.display(), original.len());

    let tmp = std::env::temp_dir().join(format!("asale-openclaw-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join(".openclaw"))?;
    std::env::set_var("HOME", &tmp);

    let path = tool_config::primary_config_path("openclaw");
    std::fs::write(&path, &original)?;
    assert!(path.starts_with(&tmp), "sanity: writing inside the throwaway home only");

    let models: Vec<String> = MODELS.iter().map(|s| s.to_string()).collect();
    let backup = tool_config::apply("openclaw", &tool_config::proxy_base(), KEY, &models)?;

    let after: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    println!("\n--- what asale added ---");
    println!("models.mode                = {}", after["models"]["mode"]);
    println!("providers.asale            = {}", after["models"]["providers"]["asale"]);
    println!("agents.defaults.model      = {}", after["agents"]["defaults"]["model"]);
    println!("\n--- what was already there ---");
    let before: serde_json::Value = serde_json::from_str(&original)?;
    for key in before.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
        let kept = after.get(&key).is_some();
        println!("{key:<12} preserved: {kept}");
        assert!(kept, "top-level key `{key}` was dropped");
    }
    let their_providers: Vec<String> = before["models"]["providers"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    for p in &their_providers {
        assert!(after["models"]["providers"].get(p).is_some(), "provider `{p}` was dropped");
    }
    println!("their providers preserved  : {their_providers:?}");
    println!("current_base_url           : {:?}", tool_config::current_base_url("openclaw"));
    println!("points_at_proxy            : {}", tool_config::points_at_proxy("openclaw"));
    assert!(tool_config::points_at_proxy("openclaw"));

    tool_config::restore("openclaw", &backup)?;
    let restored = std::fs::read_to_string(&path)?;
    assert_eq!(restored, original, "restore was not byte-exact");
    println!("\nrestore: byte-exact ({} bytes)", restored.len());

    let _ = std::fs::remove_dir_all(&tmp);
    println!("OK — the real config survives a full on/off cycle unchanged.");
    Ok(())
}
