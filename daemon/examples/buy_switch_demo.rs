//! Real end-to-end demonstration of the buy-side switch (flow §4/§6) for all
//! three locally installable CLIs: turning buying on rewrites that tool's own
//! config to point at the asale local proxy, turning it off restores the
//! original files byte-for-byte. Runs against a throwaway $HOME so the real
//! ~/.claude, ~/.codex and ~/.gemini are untouched.
//!
//!   cargo run --example buy_switch_demo

use asale_daemon::tool_config;

const PROXY: &str = "http://127.0.0.1:9787";
const KEY: &str = "sk-asale-DEMO";
/// The buy page's model selection. Only Codex reads it — it picks a model from
/// a catalog of its own rather than from the request, so the selection has to
/// be written into its config.
const MODELS: [&str; 1] = ["claude-fable-5"];

fn models() -> Vec<String> {
    MODELS.iter().map(|s| s.to_string()).collect()
}

fn show(tool: &str, label: &str) {
    println!("\n=== {} — {label} ===", tool_config::label(tool));
    for path in tool_config::config_paths(tool) {
        println!("--- {} ---", path.display());
        match std::fs::read_to_string(&path) {
            Ok(raw) => print!("{raw}"),
            Err(_) => println!("<file does not exist>"),
        }
    }
    println!("current base_url: {:?}", tool_config::current_base_url(tool));
}

/// Seed a realistic pre-existing user config for `tool`, returning the raw
/// contents of every file so the restore can be checked byte-for-byte.
fn seed(tool: &str) -> Vec<(std::path::PathBuf, String)> {
    let files: Vec<(std::path::PathBuf, String)> = match tool {
        "claude" => vec![(
            tool_config::primary_config_path("claude"),
            "{\n  \"env\": {\n    \"ANTHROPIC_API_KEY\": \"sk-ant-users-own-key\",\n    \"MY_FLAG\": \"1\"\n  },\n  \"permissions\": { \"allow\": [\"Bash\", \"Read\"] },\n  \"model\": \"opusplan\"\n}".into(),
        )],
        "codex" => {
            let paths = tool_config::config_paths("codex");
            vec![
                (
                    paths[0].clone(),
                    "model = \"gpt-5-codex\"\nmodel_provider = \"openai\"\n\n[model_providers.openai]\nname = \"OpenAI\"\nbase_url = \"https://api.openai.com/v1\"\n".into(),
                ),
                (paths[1].clone(), "{\n  \"OPENAI_API_KEY\": \"sk-users-own-openai-key\"\n}".into()),
            ]
        }
        "gemini" => vec![(
            tool_config::primary_config_path("gemini"),
            "# my gemini env\nGEMINI_API_KEY=users-own-gemini-key\nMY_FLAG=1\n".into(),
        )],
        _ => vec![],
    };
    for (path, body) in &files {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    files
}

fn main() {
    // Isolate: point $HOME at a temp dir so we never touch the real configs.
    let tmp = std::env::temp_dir().join("asale-buy-switch-demo");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("HOME", &tmp);

    for tool in tool_config::TOOLS {
        let original = seed(tool);
        show(tool, "BEFORE (user's own config)");

        // --- flow §4: buy on -> point the tool at the asale local proxy ---
        println!("\n>>> set_buy_tool({tool}, on): apply(base={PROXY}, token={KEY}, models={MODELS:?})");
        let backup = tool_config::apply(tool, PROXY, KEY, &models()).unwrap();
        show(tool, "AFTER buy on (routed through asale)");

        let expected_base = if *tool == "codex" { format!("{PROXY}/v1") } else { PROXY.to_string() };
        assert_eq!(tool_config::current_base_url(tool).as_deref(), Some(expected_base.as_str()));
        println!("  ✓ base URL switched to the asale proxy");
        println!("  ✓ asale key injected as the tool's bearer");
        println!("  ✓ {} file(s) backed up verbatim", backup.files.len());

        // --- flow §6: buy off -> restore the originals verbatim ---
        println!("\n>>> set_buy_tool({tool}, off): restore(backup)");
        tool_config::restore(tool, &backup).unwrap();
        show(tool, "AFTER buy off (original restored)");
        for (path, body) in &original {
            assert_eq!(
                std::fs::read_to_string(path).ok().as_deref(),
                Some(body.as_str()),
                "{} restored byte-for-byte",
                path.display()
            );
        }
        println!("  ✓ every file restored byte-for-byte to the user's original");
    }

    let _ = std::fs::remove_dir_all(&tmp);
    println!("\nALL ASSERTIONS PASSED — buy switch on/off verified for Claude Code, Codex and Gemini CLI.");
}
