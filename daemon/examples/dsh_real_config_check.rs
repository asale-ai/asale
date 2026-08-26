//! Dry-run of the DeepSeek Harness buy switch into a throwaway `$DSH_HOME`,
//! then hand the result to the harness itself to confirm it is accepted.
//!
//! The unit tests assert what asale *writes*. Only the harness can answer
//! whether what asale writes is what it will *read* — this file exists to close
//! that gap, because the settings document is one where a deviation is refused
//! at boot rather than skipped, which would take every other provider down with
//! it.
//!
//!   cargo run --example dsh_real_config_check
//!
//! It prints a `dsh --dump-config` command to run against the generated home.
//! A route that survives that dump is one the harness parsed and kept.
//!
//! This machine's real config is used as the starting document when it has one,
//! so the check also covers merging into whatever is already there. It is only
//! ever *read*: everything is written inside the throwaway home.

use asale_daemon::tool_config;

const KEY: &str = "sk-asale-DRYRUN";
const MODELS: [&str; 2] = ["deepseek-v4-pro", "gpt-5.6-terra"];

fn main() -> anyhow::Result<()> {
    // Resolve the real home before `$DSH_HOME` is pointed elsewhere.
    let real = std::env::var_os("DSH_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            ["HOME", "USERPROFILE"]
                .into_iter()
                .find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()))
                .map(|h| std::path::Path::new(&h).join(".dsh"))
        })
        .ok_or_else(|| anyhow::anyhow!("no DSH_HOME/HOME/USERPROFILE"))?;

    let original = std::fs::read_to_string(real.join("settings.yaml")).unwrap_or_default();
    if original.is_empty() {
        println!("source: {} — none, starting from an empty document", real.display());
    } else {
        println!("source: {}/settings.yaml ({} bytes)", real.display(), original.len());
    }

    let tmp = std::env::temp_dir().join(format!("asale-dsh-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    std::env::set_var("DSH_HOME", &tmp);

    let paths = tool_config::config_paths("dsh");
    let (settings, creds) = (&paths[0], &paths[1]);
    assert!(settings.starts_with(&tmp), "sanity: writing inside the throwaway home only");
    if !original.is_empty() {
        std::fs::write(settings, &original)?;
    }

    let models: Vec<String> = MODELS.iter().map(|s| s.to_string()).collect();
    let backup = tool_config::apply("dsh", &tool_config::proxy_base(), KEY, &models)?;

    println!("\n--- settings.yaml ---\n{}", std::fs::read_to_string(settings)?);
    println!("--- .credentials.yaml ---\n{}", std::fs::read_to_string(creds)?);
    println!("base url in effect : {:?}", tool_config::current_base_url("dsh"));
    println!("points at proxy    : {}", tool_config::points_at_proxy("dsh"));

    // The restore has to put the machine back exactly, including removing a
    // credentials file that was not there before.
    tool_config::restore("dsh", &backup)?;
    let after = std::fs::read_to_string(settings).unwrap_or_default();
    println!("\nrestored byte-exact: {}", after == original);
    println!("credentials removed: {}", !creds.exists());

    // Put it back so the dump below has something to read.
    tool_config::apply("dsh", &tool_config::proxy_base(), KEY, &models)?;
    println!(
        "\nNow ask the harness whether it accepts it:\n\
         \n    DSH_HOME={} dsh --dump-config\n\
         \nThe `asale` route appearing under `llm-pi-ai` means it parsed and kept it.",
        tmp.display()
    );
    Ok(())
}
