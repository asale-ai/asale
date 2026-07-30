//! Dry-run of the Hermes buy switch against a real Hermes config, in a
//! throwaway $HOME so nothing installed is opened for writing.
//!
//! Prefers `~/.hermes/config.yaml` when Hermes has been set up; otherwise falls
//! back to the repo's own `cli-config.yaml.example`, which is the harder case
//! anyway — 1600+ lines that are almost entirely comments, which a serde YAML
//! round-trip would erase and this editor must not touch.
//!
//!   cargo run --example hermes_real_config_check

use asale_daemon::tool_config;

const KEY: &str = "sk-asale-DRYRUN";
const MODELS: [&str; 2] = ["gpt-5.6-terra", "claude-fable-5"];

fn main() -> anyhow::Result<()> {
    let real_home = ["HOME", "USERPROFILE"]
        .into_iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()))
        .ok_or_else(|| anyhow::anyhow!("no HOME/USERPROFILE"))?;
    let home = std::path::Path::new(&real_home);
    // Where Hermes actually keeps its config: `HERMES_HOME` first (its
    // installer sets it), `~/.hermes` only as the fallback.
    let hermes_home = std::env::var("HERMES_HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".hermes"));
    let candidates = [
        hermes_home.join("config.yaml"),
        home.join("AppData/Local/hermes/config.yaml"),
        home.join("AppData/Local/hermes/hermes-agent/cli-config.yaml.example"),
    ];
    let source = candidates
        .iter()
        .find(|p| p.exists())
        .ok_or_else(|| anyhow::anyhow!("no Hermes config found in {candidates:?}"))?;
    let original = std::fs::read_to_string(source)?;
    let comment_lines = original.lines().filter(|l| l.trim_start().starts_with('#')).count();
    println!(
        "source: {} ({} bytes, {} lines, {comment_lines} of them comments)",
        source.display(),
        original.len(),
        original.lines().count()
    );

    let tmp = std::env::temp_dir().join(format!("asale-hermes-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join(".hermes"))?;
    std::env::set_var("HOME", &tmp);
    // Redirected too, or `tool_config` resolves the machine's real Hermes
    // directory and this "dry run" rewrites the config it just read.
    std::env::set_var("HERMES_HOME", tmp.join(".hermes"));

    let path = tool_config::primary_config_path("hermes");
    std::fs::write(&path, &original)?;
    assert!(path.starts_with(&tmp), "sanity: writing inside the throwaway home only");

    let models: Vec<String> = MODELS.iter().map(|s| s.to_string()).collect();
    let backup = tool_config::apply("hermes", &tool_config::proxy_base(), KEY, &models)?;
    let after = std::fs::read_to_string(&path)?;

    println!("\n--- the `model:` block afterwards ---");
    for line in after.lines().skip_while(|l| !l.starts_with("model:")).take(60) {
        if line.starts_with(|c: char| !c.is_whitespace()) && !line.starts_with("model:") {
            break;
        }
        if !line.trim().is_empty() && !line.trim_start().starts_with('#') {
            println!("{line}");
        }
    }

    let after_comments = after.lines().filter(|l| l.trim_start().starts_with('#')).count();
    println!("\ncomments kept              : {after_comments} of {comment_lines}");
    assert_eq!(after_comments, comment_lines, "a comment was lost");
    println!("current_base_url           : {:?}", tool_config::current_base_url("hermes"));
    println!("points_at_proxy            : {}", tool_config::points_at_proxy("hermes"));
    assert!(tool_config::points_at_proxy("hermes"));

    // Nothing outside the `model:` block may move — `base_url` in particular
    // also lives under `auxiliary.compression`.
    // Compared as multisets, not by line index: one inserted key shifts every
    // line after it, and an index-wise diff would report the whole file as
    // changed and hide the three lines that actually did.
    fn count(text: &str) -> std::collections::BTreeMap<&str, i64> {
        let mut m = std::collections::BTreeMap::new();
        for l in text.lines() {
            *m.entry(l).or_default() += 1;
        }
        m
    }
    let (before, now) = (count(&original), count(&after));
    let gone: Vec<&str> = before.keys().copied().filter(|l| now.get(l).copied().unwrap_or(0) < before[l]).collect();
    let added: Vec<&str> = now.keys().copied().filter(|l| before.get(l).copied().unwrap_or(0) < now[l]).collect();
    println!("\n--- lines removed ({}) ---", gone.len());
    for l in &gone {
        println!("  - {l}");
    }
    println!("--- lines added ({}) ---", added.len());
    for l in &added {
        println!("  + {l}");
    }

    tool_config::restore("hermes", &backup)?;
    let restored = std::fs::read_to_string(&path)?;
    assert_eq!(restored, original, "restore was not byte-exact");
    println!("\nrestore: byte-exact ({} bytes)", restored.len());

    // The no-backup fallback (an older build, or a lost switch record) has to
    // land on the same place without one.
    std::fs::write(&path, &after)?;
    tool_config::strip_all("hermes")?;
    let stripped = std::fs::read_to_string(&path)?;
    let leftovers: Vec<&str> = stripped
        .lines()
        .filter(|l| l.contains("asale") || l.contains(&tool_config::proxy_base()))
        .collect();
    assert!(leftovers.is_empty(), "strip left our keys behind: {leftovers:?}");
    println!("strip (no backup): no asale keys left, {} lines", stripped.lines().count());

    let _ = std::fs::remove_dir_all(&tmp);
    println!("OK");
    Ok(())
}
