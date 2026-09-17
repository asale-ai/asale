//! Dry-run of the Claude Desktop buy switch against this machine's *real*
//! `claude_desktop_config.json` — the file is copied into a throwaway $HOME
//! first, so the app's own config is never opened for writing. Prints the four
//! files the switch writes, so they can be dropped into the app's third-party
//! directory for a live check (see below).
//!
//! The unit tests cover the shapes; this covers the real config, which is where
//! the app keeps settings that must survive an on/off cycle.
//!
//!   cargo run --example claude_desktop_real_config_check
//!
//! To then verify against the app itself *without* disturbing the instance the
//! user is working in: copy only the `Claude-3p` files this prints into the
//! real `Claude-3p` directory (the normal one keeps the Claude.ai login, and
//! the mode file the app actually reads is the third-party copy), serve the two
//! endpoints on the printed proxy port, and `open -na /Applications/Claude.app`
//! — it starts as a second instance in third-party mode. Verified this way on
//! 2026-09-17: the app logged `[custom-3p] 3P mode active { provider:
//! 'gateway' }`, `picker = 2 (inferenceModels)`,
//! `inference apiHost=http://127.0.0.1:9787/claude-desktop`, and a chat turn
//! arrived as `POST /claude-desktop/v1/messages` with the role slot resolved to
//! the bought model. Afterwards: quit that instance and delete
//! `~/Library/Application Support/Claude-3p` + `~/Library/Logs/Claude-3p`.
use asale_daemon::tool_config;

const KEY: &str = "sk-asale-DRYRUN";
const MODELS: [&str; 2] = ["kimi-k2-thinking", "deepseek-v3.2"];

fn main() -> anyhow::Result<()> {
    // Read the real config through whichever variable names the home directory
    // on this platform, before $HOME is pointed elsewhere.
    let real_home = ["HOME", "USERPROFILE"]
        .into_iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()))
        .ok_or_else(|| anyhow::anyhow!("no HOME/USERPROFILE"))?;
    let real = tool_config::config_paths("claude-desktop")[0].clone();
    let original = std::fs::read_to_string(&real)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", real.display()))?;
    println!("source: {} ({} bytes)", real.display(), original.len());
    assert!(real.starts_with(&real_home), "sanity: that path is under the real home");

    let tmp = std::env::temp_dir().join(format!("asale-cd-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    std::env::set_var("HOME", &tmp);

    let paths = tool_config::config_paths("claude-desktop");
    for p in &paths {
        assert!(p.starts_with(&tmp), "sanity: writing inside the throwaway home only");
    }
    std::fs::create_dir_all(paths[0].parent().unwrap())?;
    std::fs::write(&paths[0], &original)?;

    let models: Vec<String> = MODELS.iter().map(|s| s.to_string()).collect();
    let backup = tool_config::apply("claude-desktop", "unused", KEY, &models)?;

    println!("\n--- the four files the switch writes ---");
    for p in &paths {
        println!("\n{}:\n{}", p.display(), std::fs::read_to_string(p)?);
    }

    println!("--- the app's own settings ---");
    let before: serde_json::Value = serde_json::from_str(&original)?;
    let after: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&paths[0])?)?;
    for key in before.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
        let kept = after.get(&key).is_some();
        println!("{key:<24} preserved: {kept}");
        assert!(kept, "top-level key `{key}` was dropped");
    }
    println!("current_base_url         : {:?}", tool_config::current_base_url("claude-desktop"));
    println!("points_at_proxy          : {}", tool_config::points_at_proxy("claude-desktop"));
    assert!(tool_config::points_at_proxy("claude-desktop"));

    tool_config::restore("claude-desktop", &backup)?;
    let restored = std::fs::read_to_string(&paths[0])?;
    assert_eq!(restored, original, "restore was not byte-exact");
    // Everything else the switch created has to be gone, not merely reverted.
    for p in &paths[1..] {
        assert!(!p.exists(), "{} was left behind", p.display());
    }
    println!("\nrestore: byte-exact ({} bytes), the three files we created are gone", restored.len());

    let _ = std::fs::remove_dir_all(&tmp);
    println!("OK — the real config survives a full on/off cycle unchanged.");
    Ok(())
}
