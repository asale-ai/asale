//! End-to-end coverage of the two switches the UI drives, against a real
//! `AppState` (real SQLite store, real encrypted secret store, real config
//! files) — but with `$HOME` and `$ASALE_DATA_DIR` pointed at throwaway dirs, so
//! the machine's actual `~/.claude`, `~/.codex` and `~/.gemini` are untouched.
//!
//! Covers what the unit tests cannot: that the RPC-level commands wire the
//! store, the secret store, the account pool and the on-disk configs together.
//!
//! No network: `set_buy_tool` only calls the server when it has to mint an API
//! key, so the tests seed a cached one.

use asale_client_core::store::LocalStore;
use asale_daemon::{auth_store, commands, keychain, state::AppState, tool_config};
use std::sync::Mutex;

/// `$HOME` and `$ASALE_DATA_DIR` are process-global; serialize the tests and
/// run them on the same runtime rather than in parallel.
static ENV: Mutex<()> = Mutex::new(());

struct Sandbox {
    _dir: std::path::PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("asale-it-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::env::set_var("HOME", dir.join("home"));
        std::env::set_var("ASALE_DATA_DIR", dir.join("data"));
        // Hermes is the one tool that does not live under `~/.<tool>`: on
        // Windows its config is in `%LOCALAPPDATA%\hermes`, which `HOME` does
        // not move. Without this the buy-switch tests rewrote the *developer's*
        // real Hermes config — and left it as invalid YAML, which Hermes
        // answers by ignoring the whole file. `HERMES_HOME` is the first thing
        // `tool_config::hermes_home` consults, so it moves both platforms at
        // once.
        std::env::set_var("HERMES_HOME", dir.join("home").join("hermes"));
        // The login keychain ignores `$HOME`, so the CLI scan must be told to
        // skip it — otherwise a developer's real Claude token would be read
        // into a run that should only see its sandbox.
        std::env::set_var("ASALE_DISABLE_OS_KEYCHAIN_SCAN", "1");
        // Codex's model picker is generated from the catalog its own binary
        // dumps; point that at a stub so the run neither depends on a Codex
        // being installed nor shells out to the real one.
        let stub = dir.join("home").join("codex-stub");
        std::fs::write(
            &stub,
            "#!/bin/sh\ncat <<'JSON'\n{\"models\":[\
{\"slug\":\"gpt-5.5\",\"visibility\":\"list\",\"priority\":7,\"base_instructions\":\"native\"},\
{\"slug\":\"gpt-5.2\",\"visibility\":\"list\",\"priority\":29,\"base_instructions\":\"native\"}]}\nJSON\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("ASALE_CODEX_BIN", &stub);
        Sandbox { _dir: dir }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self._dir);
    }
}

/// A signed-in state with a cached consumer key, so no server call is needed.
async fn signed_in_state() -> std::sync::Arc<AppState> {
    let state = AppState::new().await.expect("app state");
    keychain::set("access_token", "test-access-token").unwrap();
    state.store.set_setting("asale_api_key", "sk-asale-test").await.unwrap();
    std::sync::Arc::new(state)
}

#[tokio::test(flavor = "current_thread")]
async fn buy_switch_rewrites_and_restores_every_tool() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("buy");
    let state = signed_in_state().await;

    for tool in tool_config::TOOLS {
        // Seed a config the user already had, so the restore has something to
        // put back (the "created from nothing" case is covered by unit tests).
        let path = tool_config::primary_config_path(tool);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = match *tool {
            "claude" => "{\n  \"model\": \"opusplan\"\n}",
            "codex" => "model = \"gpt-5-codex\"\nmodel_provider = \"openai\"\n",
            // Each seed has to be valid in its tool's own format. Hermes reads
            // YAML, where `MY_FLAG=1` is a parse error — and the switch now
            // refuses to edit a file the tool would ignore, so the old seed was
            // testing the refusal rather than the round trip.
            "hermes" => "# mine\nmy_flag: 1\n",
            // opencode reads JSON, and its switch refuses to rewrite a file it
            // cannot round-trip — same reason as Hermes above.
            "opencode" => "{\n  \"theme\": \"tokyonight\"\n}",
            _ => "# mine\nMY_FLAG=1\n",
        };
        std::fs::write(&path, original).unwrap();

        // ── on ──
        let r = commands::set_buy_tool(&state, tool.to_string(), true, Some(vec!["claude-sonnet-4-5".into()]))
            .await
            .expect("buy on");
        assert_eq!(r["enabled"], true);
        assert_eq!(r["backed_up"], true, "{tool}: the pre-existing file was captured");

        let listed = commands::buy_tools(&state).await.unwrap();
        let entry = listed["tools"].as_array().unwrap().iter().find(|t| t["id"] == *tool).unwrap();
        assert_eq!(entry["enabled"], true, "{tool}: switch reads back on");
        assert_eq!(entry["in_effect"], true, "{tool}: live config really points at the proxy");
        assert_eq!(entry["models"][0], "claude-sonnet-4-5", "{tool}: selection persisted");
        assert!(
            std::fs::read_to_string(&path).unwrap() != original,
            "{tool}: config was actually rewritten"
        );

        // ── off ──
        commands::set_buy_tool(&state, tool.to_string(), false, None).await.expect("buy off");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "{tool}: config restored byte-for-byte"
        );
        let listed = commands::buy_tools(&state).await.unwrap();
        let entry = listed["tools"].as_array().unwrap().iter().find(|t| t["id"] == *tool).unwrap();
        assert_eq!(entry["enabled"], false);
        assert_eq!(entry["in_effect"], false);
        assert_eq!(entry["models"][0], "claude-sonnet-4-5", "selection survives switching off");
    }
}

/// A config that stopped pointing at the proxy while the switch stayed on is
/// the daemon's problem to fix, not the user's: anything can rewrite
/// `~/.claude/settings.json` (another switcher, an editor, an installer), and
/// the old advice — "toggle the switch off and on again" — asked the user to
/// perform by hand exactly what re-applying does.
///
/// The pristine backup has to survive that repair, or turning the switch off
/// would "restore" asale's own writing over the user's file.
#[tokio::test(flavor = "current_thread")]
async fn a_drifted_config_is_repaired_when_the_buy_page_loads() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("drift");
    let state = signed_in_state().await;

    let path = tool_config::primary_config_path("claude");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = "{\n  \"model\": \"opusplan\"\n}";
    std::fs::write(&path, original).unwrap();
    commands::set_buy_tool(&state, "claude".into(), true, None).await.expect("buy on");

    // Something else takes the file over — switch still on, config no longer ours.
    std::fs::write(&path, "{\n  \"env\": {\"ANTHROPIC_BASE_URL\": \"https://elsewhere.example\"}\n}").unwrap();
    assert!(!tool_config::points_at_proxy("claude"), "precondition: it really drifted");

    let listed = commands::buy_tools(&state).await.unwrap();
    let entry = listed["tools"].as_array().unwrap().iter().find(|t| t["id"] == "claude").unwrap();
    assert_eq!(entry["in_effect"], true, "repaired before the page is told about it");
    assert_eq!(listed["repaired"][0], "claude", "and the page can say what it repaired");

    // Off still means the user's own file, not the one the repair wrote.
    commands::set_buy_tool(&state, "claude".into(), false, None).await.expect("buy off");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        original,
        "the backup taken when the switch went on survived the repair"
    );
}

/// Putting an account on the market needs a session — without one the publisher
/// only loops on "session expired" behind a switch that looks on. Switching off
/// never does: a lapsed session must not trap a user into selling.
#[tokio::test(flavor = "current_thread")]
async fn selling_needs_a_session_to_start_but_not_to_stop() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("sell-auth");
    let state = signed_in_state().await;

    keychain::set(&keychain::token_ref("claude", "a@x.com"), "tok").unwrap();
    state
        .store
        .upsert_tool("claude", "a@x.com", &keychain::token_ref("claude", "a@x.com"), &["test"], "oauth")
        .await
        .unwrap();

    let on = || commands::set_account_sell(&state, "claude".into(), "a@x.com".into(), true, None, None, None, None, None);
    let off = || commands::set_account_sell(&state, "claude".into(), "a@x.com".into(), false, None, None, None, None, None);

    keychain::delete("access_token").unwrap();
    let e = on().await.expect_err("signed out: refused");
    assert_eq!(e.key.as_deref(), Some("errors.session.signInToSell"), "so the UI can send them to sign in");
    assert!(!commands::publish_wanted(&state).await, "and the switch did not move");

    keychain::set("access_token", "test-access-token").unwrap();
    on().await.expect("signed in: allowed");
    assert!(commands::publish_wanted(&state).await);

    // Session lapses while selling: stopping still works, and so does editing
    // the terms of an account that is already on.
    keychain::delete("access_token").unwrap();
    commands::set_account_sell(&state, "claude".into(), "a@x.com".into(), true, Some(500_000), None, None, None, None)
        .await
        .expect("terms of an already-selling account stay editable");
    off().await.expect("stopping never needs a session");
    assert!(!commands::publish_wanted(&state).await);
}

/// Codex takes its model from its own config, not from the caller's request,
/// so changing the buy page's selection has to rewrite that config — and doing
/// so must not lose the pristine backup taken when the switch went on.
///
/// The models are published under Codex's own slugs (the desktop app drops
/// anything else, see `codex_catalog`), so what moves with the selection is the
/// display name and the alias table, not the slug.
#[tokio::test(flavor = "current_thread")]
async fn changing_the_codex_model_rewrites_its_config_without_losing_the_backup() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("codex-model");
    let state = signed_in_state().await;

    let path = tool_config::primary_config_path("codex");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = "model = \"gpt-5-codex\"\nmodel_provider = \"openai\"\n";
    std::fs::write(&path, original).unwrap();

    let aliases = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(asale_daemon::codex_catalog::aliases_path()).unwrap()).unwrap()
    };
    let on = |models: Vec<String>| commands::set_buy_tool(&state, "codex".into(), true, Some(models));
    on(vec!["claude-fable-5".into()]).await.expect("buy on");
    assert!(
        std::fs::read_to_string(&path).unwrap().contains("model = \"gpt-5.5\""),
        "codex starts on the slug carrying the bought model"
    );
    assert_eq!(aliases()["gpt-5.5"], "claude-fable-5", "which the proxy can translate back");

    // Same switch, different model: the picker and the active model follow.
    on(vec!["claude-opus-5".into(), "claude-fable-5".into()]).await.expect("model change");
    let cfg = std::fs::read_to_string(&path).unwrap();
    assert!(cfg.contains("model = \"gpt-5.5\""), "still the best carrier");
    assert_eq!(aliases()["gpt-5.5"], "claude-opus-5", "now standing for the new first pick");
    assert_eq!(aliases()["gpt-5.2"], "claude-fable-5");
    let catalog: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(asale_daemon::codex_catalog::path()).unwrap()).unwrap();
    let listed: Vec<(&str, &str)> = catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["visibility"] == "list")
        .map(|m| (m["slug"].as_str().unwrap(), m["display_name"].as_str().unwrap()))
        .collect();
    assert_eq!(
        listed,
        [("gpt-5.5", "claude-opus-5"), ("gpt-5.2", "claude-fable-5")],
        "picker follows too"
    );

    // Off: back to exactly what the user had before asale touched anything —
    // not to the config a re-apply had already rewritten.
    commands::set_buy_tool(&state, "codex".into(), false, None).await.expect("buy off");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original, "restored byte-for-byte");
    assert!(!asale_daemon::codex_catalog::path().exists(), "generated catalog removed");
    assert!(!asale_daemon::codex_catalog::aliases_path().exists(), "and its alias table with it");
}

/// Whether this device should be on the market is *derived* from the account
/// switches — there is no device-wide sell switch that could disagree with the
/// per-account ones the user actually sees.
#[tokio::test(flavor = "current_thread")]
async fn selling_intent_follows_the_account_switches() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("intent");
    let state = signed_in_state().await;

    keychain::set(&keychain::token_ref("claude", "a@x.com"), "tok").unwrap();
    state
        .store
        .upsert_tool("claude", "a@x.com", &keychain::token_ref("claude", "a@x.com"), &["test"], "oauth")
        .await
        .unwrap();

    // A connected-but-switched-off account keeps the device off the market.
    assert!(!commands::publish_wanted(&state).await);
    let st = commands::client_status(&state).await.unwrap();
    assert_eq!(st["accounts_total"], 1);
    assert_eq!(st["selling"].as_array().unwrap().len(), 0);
    assert_eq!(st["publish_state"], "offline");
    assert_eq!(st["signed_in"], true);

    // Switching the first account on is the whole gesture: nothing else has to
    // be armed for this device to want to be selling.
    commands::set_account_sell(&state, "claude".into(), "a@x.com".into(), true, None, None, None, None, None).await.unwrap();
    assert!(commands::publish_wanted(&state).await);
    assert_eq!(commands::proxy_status(&state).await.unwrap()["publish_wanted"], true);
    assert_eq!(
        commands::client_status(&state).await.unwrap()["selling"].as_array().unwrap().len(),
        1
    );

    // Switching the last one off takes the device back off the market, session
    // included — a device with nothing to sell must not hold a live session.
    commands::set_account_sell(&state, "claude".into(), "a@x.com".into(), false, None, None, None, None, None).await.unwrap();
    assert!(!commands::publish_wanted(&state).await);
    assert_eq!(commands::client_status(&state).await.unwrap()["publish_state"], "offline");
}

/// Two accounts of one provider must stay fully independent: switching one on
/// leaves the other off, each keeps its own cap, and metering is per account.
#[tokio::test(flavor = "current_thread")]
async fn selling_is_per_account_and_isolated_from_the_local_cli() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("sell");
    let state = signed_in_state().await;

    // One account asale owns outright, one copied from the local CLI.
    for (account, origin) in [("owned@x.com", "oauth"), ("shared@x.com", "import")] {
        keychain::set(&keychain::token_ref("claude", account), "tok").unwrap();
        state
            .store
            .upsert_tool("claude", account, &keychain::token_ref("claude", account), &["test"], origin)
            .await
            .unwrap();
        state.store.set_setting(&format!("plan:claude:{account}"), "max_20x").await.unwrap();
    }

    // Both start off — connecting an account must never start selling it.
    let before = commands::list_accounts(&state).await.unwrap();
    assert_eq!(before.as_array().unwrap().len(), 2);
    assert!(
        before.as_array().unwrap().iter().all(|a| a["sell_enabled"] == false),
        "a newly connected account is not sold until switched on"
    );

    // Switch on exactly one, with its own daily cap; leave the other off.
    commands::set_account_sell(&state, "claude".into(), "owned@x.com".into(), true, Some(500_000), None, None, None, None)
        .await
        .unwrap();
    commands::set_account_sell(&state, "claude".into(), "shared@x.com".into(), false, None, None, None, None, None)
        .await
        .unwrap();

    let rows = commands::list_accounts(&state).await.unwrap();
    let get = |id: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|a| a["account_id"] == id)
            .cloned()
            .unwrap()
    };
    let owned = get("owned@x.com");
    let shared = get("shared@x.com");

    assert_eq!(owned["sell_enabled"], true);
    assert_eq!(owned["sell_daily_limit"], 500_000);
    assert_eq!(shared["sell_enabled"], false, "the sibling account is unaffected");
    assert_eq!(shared["sell_daily_limit"], 0);

    // Origin drives the UI's "shared with the CLI you use locally" warning.
    assert_eq!(owned["origin"], "oauth");
    assert_eq!(owned["shared_with_local_cli"], false);
    assert_eq!(shared["origin"], "import");
    assert_eq!(shared["shared_with_local_cli"], true);

    // Each account has its own manifest in asale's directory — never the CLI's.
    let manifests = auth_store::list();
    assert_eq!(manifests.len(), 2, "one manifest per account");
    assert!(
        auth_store::auth_dir().starts_with(std::env::var("ASALE_DATA_DIR").unwrap()),
        "credentials stay under asale's data dir"
    );
    let m = manifests.iter().find(|m| m.account_id == "owned@x.com").unwrap();
    assert!(m.sell_enabled && m.sell_daily_limit == 500_000, "manifest mirrors the sell state");

    // Metering is attributed per account: usage served by one must not count
    // against the other's daily cap.
    state
        .store
        .insert_provider_record("task-1", "claude", "owned@x.com", "claude-sonnet-4-5", 100, 50, 0, 0, "ok")
        .await
        .unwrap();
    assert_eq!(
        state.store.served_tokens_today_for_account("claude", "owned@x.com").await.unwrap(),
        150
    );
    assert_eq!(
        state.store.served_tokens_today_for_account("claude", "shared@x.com").await.unwrap(),
        0,
        "the sibling's counter is untouched"
    );

    let rows = commands::list_accounts(&state).await.unwrap();
    assert_eq!(get_from(&rows, "owned@x.com")["used_today"], 150);
    assert_eq!(get_from(&rows, "shared@x.com")["used_today"], 0);

    // Removing an account drops its manifest and leaves the sibling alone.
    commands::remove_account(&state, "claude".into(), "shared@x.com".into()).await.unwrap();
    let left = auth_store::list();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].account_id, "owned@x.com");
}

fn get_from(rows: &serde_json::Value, id: &str) -> serde_json::Value {
    rows.as_array().unwrap().iter().find(|a| a["account_id"] == id).cloned().unwrap()
}

/// A credential store carries no identity for Claude, so the importer matches
/// it against the accounts already on file. It must land on the account's real
/// identity and never on the nameless `<provider>-cli` placeholder that holds
/// the very same token — otherwise the duplicate would keep resurrecting
/// itself and the real row would be orphaned.
#[tokio::test(flavor = "current_thread")]
async fn identity_prefers_a_real_account_over_the_nameless_placeholder() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("placeholder");
    let state = signed_in_state().await;

    // Both rows hold the same login: one named, one not.
    for account in ["claude-cli", "real@x.com"] {
        keychain::set(&keychain::token_ref("claude", account), "access").unwrap();
        keychain::set(&keychain::refresh_ref("claude", account), "rt-shared").unwrap();
        state
            .store
            .upsert_tool("claude", account, &keychain::token_ref("claude", account), &["import"], "import")
            .await
            .unwrap();
    }

    let home = std::env::var("HOME").unwrap();
    std::fs::create_dir_all(format!("{home}/.claude")).unwrap();
    std::fs::write(
        format!("{home}/.claude/.credentials.json"),
        serde_json::json!({"claudeAiOauth": {
            "accessToken": "access", "refreshToken": "rt-shared",
            "expiresAt": 4_000_000_000_000i64, "subscriptionType": "max"
        }})
        .to_string(),
    )
    .unwrap();

    let r = commands::import_from_cli(&state, "claude".into()).await.expect("import");
    let accounts = r["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["account_id"], "real@x.com", "resolved to the named account, not the placeholder");
    assert_eq!(r["dropped"][0], "claude-cli");

    let rows = commands::list_accounts(&state).await.unwrap();
    let claude: Vec<_> = rows.as_array().unwrap().iter().filter(|a| a["provider"] == "claude").collect();
    assert_eq!(claude.len(), 1, "one subscription account, one row");
    assert_eq!(claude[0]["account_id"], "real@x.com");
}

/// An unsigned JWT carrying the given claims — enough for the import path,
/// which reads claims for display metadata and never verifies them.
fn fake_jwt(claims: serde_json::Value) -> String {
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    format!("{}.{}.{}", b64(br#"{"alg":"none"}"#), b64(claims.to_string().as_bytes()), b64(b"sig"))
}

/// Selling is per *subscription account*, so importing must produce one record
/// per account — and the placeholder row older builds filed under
/// `<provider>-cli`, for a credential whose account they couldn't name, is that
/// same subscription listed a second time. Importing must retire it (carrying
/// its sell switch over) instead of leaving one account sellable twice.
#[tokio::test(flavor = "current_thread")]
async fn import_retires_the_placeholder_row_of_an_account_it_can_now_name() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("identity");
    let state = signed_in_state().await;

    // The legacy row: same subscription, filed before its identity was known,
    // and switched on for selling with a cap of its own.
    keychain::set(&keychain::token_ref("codex", "codex-cli"), "old-access").unwrap();
    keychain::set(&keychain::refresh_ref("codex", "codex-cli"), "rt-1").unwrap();
    state
        .store
        .upsert_tool("codex", "codex-cli", &keychain::token_ref("codex", "codex-cli"), &["import"], "import")
        .await
        .unwrap();
    state.store.set_tool_sell("codex", "codex-cli", true, 250_000).await.unwrap();

    // The credential the local CLI actually holds — same login, but this time
    // its id_token names the account.
    let home = std::env::var("HOME").unwrap();
    std::fs::create_dir_all(format!("{home}/.codex")).unwrap();
    let auth = serde_json::json!({
        "tokens": {
            "id_token": fake_jwt(serde_json::json!({
                "email": "dev@example.com",
                "https://api.openai.com/auth": {"chatgpt_plan_type": "plus"}
            })),
            "access_token": fake_jwt(serde_json::json!({"exp": 4_000_000_000i64})),
            "refresh_token": "rt-1"
        }
    });
    std::fs::write(format!("{home}/.codex/auth.json"), auth.to_string()).unwrap();

    let r = commands::import_from_cli(&state, "codex".into()).await.expect("import");
    let accounts = r["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1, "one subscription account → one imported record");
    assert_eq!(accounts[0]["account_id"], "dev@example.com");
    assert_eq!(
        accounts[0]["sources"].as_array().unwrap().len(),
        1,
        "every store holding this account is listed — here, just the auth file"
    );
    assert!(accounts[0]["source"].as_str().unwrap().ends_with(".codex/auth.json"));
    assert_eq!(r["dropped"][0], "codex-cli", "the placeholder row was retired");

    // The sell side now sees exactly one codex account, not two.
    let rows = commands::list_accounts(&state).await.unwrap();
    let codex: Vec<_> = rows.as_array().unwrap().iter().filter(|a| a["provider"] == "codex").collect();
    assert_eq!(codex.len(), 1, "the same subscription is never listed twice");
    assert_eq!(codex[0]["account_id"], "dev@example.com");
    assert_eq!(codex[0]["sell_enabled"], true, "selling was not silently switched off by the rename");
    assert_eq!(codex[0]["sell_daily_limit"], 250_000, "and its cap came with it");
    assert_eq!(codex[0]["sources"].as_array().unwrap().len(), 1);

    // One manifest on disk too — the placeholder's file is gone.
    let manifests: Vec<_> = auth_store::list().into_iter().filter(|m| m.provider == "codex").collect();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].account_id, "dev@example.com");
    assert!(!manifests[0].sources.is_empty(), "the manifest records where the token was found");

    // Re-importing is idempotent: still one record, no placeholder to drop.
    let again = commands::import_from_cli(&state, "codex".into()).await.expect("re-import");
    assert_eq!(again["accounts"].as_array().unwrap().len(), 1);
    assert!(again["dropped"].as_array().unwrap().is_empty());
}

/// A tool pointed at the asale proxy is buying, and nothing synced out of its
/// directory belongs on the sell side while that is true. The account has to go
/// the moment the switch flips — not at the next scan — and come back with the
/// switch and cap the user left on it, since turning buying on for an afternoon
/// must not quietly cost them their sell settings.
#[tokio::test(flavor = "current_thread")]
async fn a_tool_that_is_buying_is_not_a_source_of_sellable_accounts() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("buying-not-sellable");
    let state = signed_in_state().await;

    // Seed the account first so the import resolves its identity from the token
    // it already holds — no profile call, so the test stays offline.
    keychain::set(&keychain::token_ref("claude", "me@x.com"), "access").unwrap();
    keychain::set(&keychain::refresh_ref("claude", "me@x.com"), "rt-1").unwrap();
    state
        .store
        .upsert_tool("claude", "me@x.com", &keychain::token_ref("claude", "me@x.com"), &["import"], "import")
        .await
        .unwrap();

    let home = std::env::var("HOME").unwrap();
    std::fs::create_dir_all(format!("{home}/.claude")).unwrap();
    std::fs::write(
        format!("{home}/.claude/.credentials.json"),
        serde_json::json!({"claudeAiOauth": {
            "accessToken": "access", "refreshToken": "rt-1",
            "expiresAt": 4_000_000_000_000i64, "subscriptionType": "max"
        }})
        .to_string(),
    )
    .unwrap();

    let r = commands::import_from_cli(&state, "claude".into()).await.expect("import");
    assert_eq!(r["accounts"].as_array().unwrap().len(), 1);
    commands::set_account_sell(&state, "claude".into(), "me@x.com".into(), true, Some(500_000), None, None, None, None)
        .await
        .unwrap();

    // ── buying on ──
    commands::set_buy_tool(&state, "claude".into(), true, None).await.expect("buy on");
    let rows = commands::list_accounts(&state).await.unwrap();
    assert!(
        rows.as_array().unwrap().iter().all(|a| a["provider"] != "claude"),
        "a tool that is buying offers nothing for sale"
    );
    assert!(
        auth_store::list().iter().all(|m| m.provider != "claude"),
        "and its manifest goes too, so the pool cannot serve from it"
    );

    // A rescan must not bring it back — the credential is still there to find.
    let all = commands::import_cli_all(&state).await.expect("rescan");
    assert!(all["imported"].as_array().unwrap().is_empty(), "nothing imported while buying");
    let skipped = all["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["provider"], "claude");
    assert_eq!(skipped[0]["reason"], "buying");

    // ── buying off ──
    // Coming back onto the market is settled behind the answer (it reconnects
    // the publisher), so this is an eventual assertion — the switch itself has
    // already restored the config by the time the call returns.
    commands::set_buy_tool(&state, "claude".into(), false, None).await.expect("buy off");
    for _ in 0..100 {
        if auth_store::list().iter().any(|m| m.account_id == "me@x.com") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let back = get_from(&commands::list_accounts(&state).await.unwrap(), "me@x.com");
    assert_eq!(back["sell_enabled"], true, "under the identity and switch it was hidden with");
    assert_eq!(back["sell_daily_limit"], 500_000, "and the cap");
    assert!(auth_store::list().iter().any(|m| m.account_id == "me@x.com"), "manifest is back too");
}

/// The sharpest case: with the buy switch on and no ChatGPT login of its own,
/// `~/.codex/auth.json` holds nothing but asale's *own* consumer key. Importing
/// it would put that key up for sale — the market buying its own key back.
#[tokio::test(flavor = "current_thread")]
async fn the_consumer_key_a_buy_switch_writes_is_never_offered_for_sale() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("no-selling-our-own-key");
    let state = signed_in_state().await;

    commands::set_buy_tool(&state, "codex".into(), true, None).await.expect("buy on");
    let auth = std::fs::read_to_string(format!("{}/.codex/auth.json", std::env::var("HOME").unwrap())).unwrap();
    assert!(auth.contains("sk-asale-test"), "precondition: our key is the only credential there");

    let all = commands::import_cli_all(&state).await.expect("scan");
    assert!(all["imported"].as_array().unwrap().is_empty());
    assert!(
        commands::list_accounts(&state).await.unwrap().as_array().unwrap().is_empty(),
        "our own consumer key never becomes a sellable account"
    );
}

/// An account that reaches its daily cap must stop being offered, while its
/// sibling keeps serving (auto-stop is per account, not per provider).
#[tokio::test(flavor = "current_thread")]
async fn daily_cap_stops_only_the_account_that_hit_it() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("cap");
    let state = signed_in_state().await;

    for account in ["a@x.com", "b@x.com"] {
        keychain::set(&keychain::token_ref("claude", account), "tok").unwrap();
        state
            .store
            .upsert_tool("claude", account, &keychain::token_ref("claude", account), &["test"], "oauth")
            .await
            .unwrap();
        state.store.set_setting(&format!("plan:claude:{account}"), "max_20x").await.unwrap();
        commands::set_account_sell(&state, "claude".into(), account.into(), true, Some(1_000), None, None, None, None).await.unwrap();
    }

    // Account "a" blows through its 1k daily cap.
    state
        .store
        .insert_provider_record("t1", "claude", "a@x.com", "claude-sonnet-4-5", 900, 300, 0, 0, "ok")
        .await
        .unwrap();
    asale_daemon::publisher::rebuild_pool(&state.store, &state.pool).await;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut pool = state.pool.lock().unwrap();
    // Only "b" may be picked for sale now.
    let picked = pool
        .pick_for_sale("claude", "claude-opus-5", None, now)
        .expect("a sellable account remains");
    assert_eq!(picked.account_id, "b@x.com", "the capped account is skipped");
    drop(pool);

    let rows = commands::list_accounts(&state).await.unwrap();
    assert_eq!(get_from(&rows, "a@x.com")["status"], "exhausted", "capped account reads exhausted");
    assert_eq!(get_from(&rows, "b@x.com")["status"], "available");
}

/// An older install that subscribed through the Claude-only flow must keep
/// working after upgrade — otherwise the proxy's buy gate refuses its traffic.
#[tokio::test(flavor = "current_thread")]
async fn legacy_subscription_migrates_to_the_buy_switch() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("migrate");
    let state = signed_in_state().await;

    let path = tool_config::primary_config_path("claude");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = "{\n  \"model\": \"opusplan\"\n}";
    // Simulate the old build's state: config already switched, backup in settings.
    std::fs::write(&path, "{\n  \"env\": { \"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:9787\" }\n}").unwrap();
    state.store.set_setting("cc_sub_active", "1").await.unwrap();
    state.store.set_setting("cc_claude_existed", "1").await.unwrap();
    state.store.set_setting("cc_claude_backup", original).await.unwrap();
    state.store.set_setting("cc_sub_model", "claude-sonnet-4-5").await.unwrap();

    assert!(commands::migrate_legacy_subscription(&state).await.unwrap(), "migration ran");
    assert!(
        !commands::migrate_legacy_subscription(&state).await.unwrap(),
        "migration is idempotent"
    );

    let listed = commands::buy_tools(&state).await.unwrap();
    let claude = listed["tools"].as_array().unwrap().iter().find(|t| t["id"] == "claude").unwrap();
    assert_eq!(claude["enabled"], true, "buying stays on across the upgrade");
    assert_eq!(claude["models"][0], "claude-sonnet-4-5", "the old single model seeds the multi-select");

    // And turning it off still restores the pre-subscription file exactly.
    commands::set_buy_tool(&state, "claude".into(), false, None).await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}

/// Guard rails at the command boundary.
#[tokio::test(flavor = "current_thread")]
async fn invalid_input_is_rejected_before_touching_anything() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("guards");
    let state = std::sync::Arc::new(AppState::new().await.unwrap());

    assert!(commands::set_buy_tool(&state, "emacs".into(), true, None).await.is_err());
    // Not signed in: must refuse before rewriting any config file.
    let path = tool_config::primary_config_path("claude");
    assert!(commands::set_buy_tool(&state, "claude".into(), true, None).await.is_err());
    assert!(!path.exists(), "a refused buy-on never creates a config file");
    // Unknown account.
    assert!(commands::set_account_sell(&state, "claude".into(), "nobody@x".into(), true, None, None, None, None, None)
        .await
        .is_err());

    let store = LocalStore::open_memory().await.unwrap();
    assert!(!store.set_tool_sell("claude", "ghost", true, 1).await.unwrap(), "no row to update");
}

/// A model that keeps failing must stop being sold on its own, without taking
/// the account's other models with it, and must stay stopped across a pool
/// rebuild until the operator resumes it (spec §4.5).
#[tokio::test(flavor = "current_thread")]
async fn a_broken_model_stops_selling_and_waits_for_the_operator() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("lane");
    let state = signed_in_state().await;

    let account = "a@x.com";
    keychain::set(&keychain::token_ref("claude", account), "tok").unwrap();
    state
        .store
        .upsert_tool("claude", account, &keychain::token_ref("claude", account), &["test"], "oauth")
        .await
        .unwrap();
    state.store.set_setting(&format!("plan:claude:{account}"), "max_20x").await.unwrap();
    commands::set_account_sell(&state, "claude".into(), account.into(), true, None, None, None, None, None).await.unwrap();

    let opus = "claude-opus-5";
    let haiku = "claude-haiku-4-5";
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // Everything is on the market to begin with.
    let items = asale_daemon::publisher::build_supply_items(&state.store, &state.pool).await;
    let of = |v: &serde_json::Value, model: &str| {
        v.as_array()
            .unwrap()
            .iter()
            .find(|i| i["model"] == model)
            .cloned()
            .unwrap_or_else(|| panic!("{model} missing from the declaration"))
    };
    assert_eq!(of(&items, opus)["available"], true);
    assert_eq!(of(&items, haiku)["available"], true);

    // Three consecutive upstream failures on Opus trip its breaker.
    {
        let mut pool = state.pool.lock().unwrap();
        for i in 0..3 {
            pool.pick_for_sale("claude", opus, None, now + i * 1_000).unwrap();
            pool.on_error(
                "claude",
                account,
                opus,
                asale_client_core::UpstreamErrorKind::ServerError,
                "upstream 503",
                now + i * 1_000,
            );
        }
    }

    // Opus leaves the market with a reason attached; Haiku never noticed.
    let items = asale_daemon::publisher::build_supply_items(&state.store, &state.pool).await;
    assert_eq!(of(&items, opus)["available"], false);
    assert_eq!(of(&items, opus)["paused_reason"], "breaker");
    assert_eq!(of(&items, opus)["resume_at"], 0, "a breaker has no automatic return time");
    assert_eq!(of(&items, haiku)["available"], true, "one model's failures must not stop the others");

    let lanes = commands::list_lanes(&state).await.unwrap();
    let opus_lane = lanes["lanes"].as_array().unwrap().iter().find(|l| l["model"] == opus).unwrap();
    assert_eq!(opus_lane["status"], "paused");
    assert_eq!(opus_lane["requires_user"], true, "the UI must offer a resume button");
    assert_eq!(opus_lane["last_error"], "upstream 503");

    // A rebuild (which runs every minute) must not un-pause it either.
    asale_daemon::publisher::rebuild_pool(&state.store, &state.pool).await;
    let lanes = commands::list_lanes(&state).await.unwrap();
    let opus_lane = lanes["lanes"].as_array().unwrap().iter().find(|l| l["model"] == opus).unwrap();
    assert_eq!(opus_lane["status"], "paused", "a rebuild must not un-pause a broken lane");

    // And it outlives the process. `spawn_lane_loop` writes the pause to the
    // store; a restart is a brand-new pool, and without the restore below a
    // quick `asaled` bounce would put broken capacity straight back on sale.
    state
        .store
        .set_lane_pause("claude", account, opus, "breaker", "upstream 503", now)
        .await
        .unwrap();
    let restarted = std::sync::Arc::new(std::sync::Mutex::new(asale_client_core::AccountPool::new(
        asale_client_core::Strategy::RoundRobin,
    )));
    asale_daemon::publisher::rebuild_pool(&state.store, &restarted).await;
    let restored = asale_daemon::publisher::build_supply_items(&state.store, &restarted).await;
    assert_eq!(of(&restored, opus)["available"], false, "a restart must not resume a broken lane");
    assert_eq!(of(&restored, haiku)["available"], true);

    // The operator fixes it and resumes.
    commands::resume_lane(&state, "claude".into(), account.into(), opus.into()).await.unwrap();
    let items = asale_daemon::publisher::build_supply_items(&state.store, &state.pool).await;
    assert_eq!(of(&items, opus)["available"], true, "resume must put the lane back on the market");
    assert!(state.store.list_lane_pauses().await.unwrap().is_empty(), "and forget the persisted pause");
}

/// The consumer API key is scoped to the deployment that issued it, but
/// `~/.asale/asale.db` is not: a dev build pointed at localhost and the
/// packaged one pointed at api.asale.ai share the same store on one machine.
/// A key carried across that line is an `unknown api key` 401 on every market
/// request, so it must be discarded rather than loaded.
#[tokio::test(flavor = "current_thread")]
async fn a_key_from_another_deployment_is_not_reused() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("keyorigin");
    let state = signed_in_state().await;
    let here = state.cfg.server_api_base.clone();

    // Minted here → loaded.
    state.store.set_setting("asale_api_key_origin", &here).await.unwrap();
    assert!(commands::load_api_key(&state).await.unwrap());
    assert_eq!(state.asale_key.read().await.as_deref(), Some("sk-asale-test"));

    // Minted somewhere else → dropped, and not left behind for the next start.
    state.store.set_setting("asale_api_key_origin", "https://somewhere.else").await.unwrap();
    assert!(!commands::load_api_key(&state).await.unwrap(), "a foreign key must not be loaded");
    assert_eq!(state.store.get_setting("asale_api_key").await.unwrap().unwrap_or_default(), "");
    assert!(state.asale_key.read().await.is_none());

    // A key cached before this stamp existed is kept and stamped, so upgrading
    // does not force everyone through a needless re-mint.
    state.store.set_setting("asale_api_key", "sk-asale-old").await.unwrap();
    state.store.set_setting("asale_api_key_origin", "").await.unwrap();
    assert!(commands::load_api_key(&state).await.unwrap());
    assert_eq!(state.store.get_setting("asale_api_key_origin").await.unwrap().unwrap(), here);

    // Signing out takes the key with it: it belongs to that account.
    commands::logout(&state).await.unwrap();
    assert_eq!(state.store.get_setting("asale_api_key").await.unwrap().unwrap_or_default(), "");
    assert!(state.asale_key.read().await.is_none());
}

/// Regenerating the consumer key revokes the one the buying tools are holding,
/// so the new one has to land in their configs — otherwise every tool pointed
/// at the proxy starts answering `401 unknown api key` and nothing on screen
/// explains why.
#[tokio::test(flavor = "current_thread")]
async fn regenerating_the_key_rewrites_every_buying_tool() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("rekey");
    let state = signed_in_state().await;

    let original = "{\n  \"model\": \"opusplan\"\n}";
    let claude_cfg = tool_config::primary_config_path("claude");
    std::fs::create_dir_all(claude_cfg.parent().unwrap()).unwrap();
    std::fs::write(&claude_cfg, original).unwrap();
    commands::set_buy_tool(&state, "claude".into(), true, None).await.unwrap();
    assert!(std::fs::read_to_string(&claude_cfg).unwrap().contains("sk-asale-test"));

    // Gemini stays off, and must not be rewritten on its behalf.
    let gemini_cfg = tool_config::primary_config_path("gemini");

    let touched = commands::refresh_buy_tool_keys(&state, "sk-asale-fresh").await.unwrap();
    assert_eq!(touched, vec!["claude".to_string()], "only the tools that are buying");
    let now = std::fs::read_to_string(&claude_cfg).unwrap();
    assert!(now.contains("sk-asale-fresh"), "the new key is in the config");
    assert!(!now.contains("sk-asale-test"), "the revoked one is gone");
    assert!(!gemini_cfg.exists(), "a tool that is not buying is left alone");

    // And the restore still puts back what the user had, not asale's own
    // writing — the re-key must not have overwritten the switch-on backup.
    commands::set_buy_tool(&state, "claude".into(), false, None).await.unwrap();
    assert_eq!(std::fs::read_to_string(&claude_cfg).unwrap(), original);
}

/// A model the market prices outside the account's band leaves the market, and
/// says why — while the account's other models keep selling.
///
/// This is the sell-side price floor: the seller declares what fraction of list
/// price they are willing to trade at, and the device stops advertising the
/// lanes the market has moved past. Getting the declaration right matters as
/// much as getting the local decision right — a lane that is withheld locally
/// but still indexed on the gateway is a lane that will be dispatched work at
/// exactly the price its operator refused.
#[tokio::test(flavor = "current_thread")]
async fn a_model_priced_outside_the_band_leaves_the_market() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("band");
    let state = signed_in_state().await;

    let account = "a@x.com";
    let opus = "claude-opus-5";
    let haiku = "claude-haiku-4-5";
    keychain::set(&keychain::token_ref("claude", account), "tok").unwrap();
    state
        .store
        .upsert_tool("claude", account, &keychain::token_ref("claude", account), &["test"], "oauth")
        .await
        .unwrap();
    state.store.set_setting(&format!("plan:claude:{account}"), "max_20x").await.unwrap();

    // The market: Opus trades at 38% of list, Haiku at 85%. Written straight
    // into the cache the publisher pulls into, so the test needs no server —
    // stamped *now*, because a stale cache is exactly what makes the publisher
    // go and fetch the real one, and this assertion is about the band, not
    // about whatever the live market happens to be paying today.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    state
        .store
        .set_setting(
            "sellable_catalog",
            &serde_json::json!({
                "fetched_at": now,
                "priced_at": now,
                "by_provider": {"claude": [opus, haiku]},
                "ratios": {opus: 38, haiku: 85},
            })
            .to_string(),
        )
        .await
        .unwrap();

    // Sell, but never below 60% of list price.
    commands::set_account_sell(&state, "claude".into(), account.into(), true, None, Some(60), Some(100), None, None)
        .await
        .unwrap();

    let items = asale_daemon::publisher::build_supply_items(&state.store, &state.pool).await;
    let items = items.as_array().cloned().unwrap();
    let of = |model: &str| {
        items
            .iter()
            .find(|i| i["model"] == model)
            .cloned()
            .unwrap_or_else(|| panic!("{model} missing from the declaration"))
    };
    assert_eq!(of(opus)["available"], false, "38% of list is under this account's floor");
    assert_eq!(of(opus)["paused_reason"], "price");
    assert_eq!(of(haiku)["available"], true, "85% of list is inside the band");

    let lanes = commands::list_lanes(&state).await.unwrap();
    let lane = |model: &str| {
        lanes["lanes"].as_array().unwrap().iter().find(|l| l["model"] == model).cloned().unwrap()
    };
    assert_eq!(lane(opus)["status"], "withheld");
    assert_eq!(lane(opus)["ratio"], 38, "the sell page ranks models by this");
    assert_eq!(
        lane(opus)["requires_user"],
        false,
        "a price the operator set is not something they have to come back and fix"
    );
    assert_eq!(lane(haiku)["status"], "selling");

    // Widening the band puts it straight back on: raising your own floor is a
    // decision, not a market move, so it does not wait out the re-entry dwell.
    commands::set_account_sell(&state, "claude".into(), account.into(), true, None, Some(20), Some(100), None, None)
        .await
        .unwrap();
    let items = asale_daemon::publisher::build_supply_items(&state.store, &state.pool).await;
    let opus_item = items.as_array().unwrap().iter().find(|i| i["model"] == opus).cloned().unwrap();
    assert_eq!(opus_item["available"], true, "a widened band sells again at once");
}

// ── Internal custom endpoints ──────────────────────────────────────────────

/// A one-shot HTTP stub that answers `GET /models` with an OpenAI-style list,
/// then keeps serving until the test drops it. Returns its base URL.
///
/// Real socket rather than a mocked client: the point of the probe is that it
/// talks to something that answers like an OpenAI-compatible endpoint, and a
/// stubbed transport would assert the code against itself.
async fn models_stub(ids: &[&str]) -> String {
    let body = format!(
        "{{\"data\":[{}]}}",
        ids.iter().map(|id| format!("{{\"id\":\"{id}\"}}")).collect::<Vec<_>>().join(",")
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let body = body.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}/v1")
}

/// Say what the server would answer about what this account may connect,
/// without asking it.
///
/// `commands::capabilities` caches the answer for a minute, so seeding that
/// cache is the same thing as the server having just replied — and these tests
/// have no server. `Some(true)` grants every family including the ones a stock
/// build does not draw, `Some(false)` grants only those, and `None` is "nobody
/// answered", which is the one state that must never delete anybody's keys.
async fn as_operator(state: &AppState, verdict: Option<bool>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let answer = verdict.map(|operator| commands::Capabilities {
        providers: asale_protocol::PROVIDERS
            .iter()
            .filter(|s| operator || s.offered_by_default)
            .map(|s| s.id.to_string())
            .collect(),
        sections: serde_json::json!([]),
    });
    *state.capabilities.write().await = Some((now, answer));
}

/// Seed the catalog the pool builds lanes from, as a market pull would have.
async fn seed_catalog(store: &LocalStore, by_provider: serde_json::Value) {
    let catalog = serde_json::json!({"fetched_at": 1, "by_provider": by_provider});
    store.set_setting("sellable_catalog", &catalog.to_string()).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn a_custom_endpoint_sells_the_catalog_it_can_serve_under_market_ids() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("custom");
    let state = signed_in_state().await;
    as_operator(&state, Some(true)).await;
    // What the platform trades. `claude-haiku-4-5` is the interesting one: the
    // endpoint below spells it `anthropic/claude-haiku-4.5`.
    seed_catalog(
        &state.store,
        serde_json::json!({"claude": ["claude-haiku-4-5"], "codex": ["gpt-5.5"]}),
    )
    .await;
    let base = models_stub(&["anthropic/claude-haiku-4.5", "openai/gpt-5.5", "meta/llama-4"]).await;

    // Floor at the bottom of the range — "sell at whatever the market pays".
    // A real floor would make this test depend on the live price of a real
    // model: the daemon's price loop pulls the market's actual ratios in the
    // background, and a lane priced under its floor is *correctly* withheld,
    // which has nothing to do with what this test is about. The floor's own
    // behaviour is covered where it belongs, by the price-band tests.
    let r = commands::connect_custom_endpoint(
        &state,
        base.clone(),
        "sk-endpoint-key".into(),
        None,
        Some("house".into()),
        Some(5),
        Some(12),
        None,
    )
    .await
    .expect("connect");

    assert_eq!(r["provider"], "custom");
    assert_eq!(r["account_id"], "house");
    assert_eq!(r["endpoint_models"], 3, "the probe saw the whole menu");
    assert_eq!(r["sell_enabled"], true);
    let sellable: Vec<String> = serde_json::from_value(r["sellable_models"].clone()).unwrap();
    // Traded *and* served, under the market's spelling. `llama-4` is served and
    // not traded; nothing else in the catalog is traded and not served.
    assert_eq!(sellable, vec!["claude-haiku-4-5".to_string(), "gpt-5.5".to_string()]);

    // The terms landed on the account exactly as a subscription's would.
    let tool = state
        .store
        .list_tools()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.provider == "custom" && t.account_id == "house")
        .expect("account row");
    assert_eq!(tool.sell_min_ratio, 5, "price floor");
    assert_eq!(tool.sell_concurrency, 12, "concurrency ceiling");
    assert!(tool.sell_enabled);

    // And the declaration the market would receive carries that ceiling, under
    // the market's model ids.
    let items = asale_daemon::publisher::build_supply_items(&state.store, &state.pool).await;
    let items = items.as_array().unwrap();
    let haiku = items
        .iter()
        .find(|i| i["model"] == "claude-haiku-4-5")
        .expect("the lane is declared");
    assert_eq!(haiku["provider"], "custom");
    assert_eq!(haiku["concurrency_free"], 12, "the seller's own ceiling, not a constant");
    assert_eq!(haiku["available"], true);
    // The stub answers an OpenAI model list to anyone, so that is what the
    // probe found — and the lane says so, because the gateway builds this
    // lane's body from that answer and nothing else tells it.
    assert_eq!(r["wire"], "openai");
    assert_eq!(haiku["wire"], "openai");

}

/// A models endpoint that answers only when the key arrives in `header`, the
/// way an Anthropic-compatible host insists on `x-api-key` and 401s a bearer.
/// Same reason as [`models_stub`] for using a real socket: the probe's whole
/// job is to find out which of these a host is.
async fn models_stub_keyed_by(header: &'static str, ids: &[&str]) -> String {
    let body = format!(
        "{{\"data\":[{}]}}",
        ids.iter().map(|id| format!("{{\"id\":\"{id}\"}}")).collect::<Vec<_>>().join(",")
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let body = body.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let resp = if req.contains(&format!("{header}:")) {
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else {
                    "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}/v1")
}

#[tokio::test(flavor = "current_thread")]
async fn an_endpoint_that_speaks_anthropic_is_found_and_sold_as_such() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("custom-wire");
    let state = signed_in_state().await;
    as_operator(&state, Some(true)).await;
    seed_catalog(&state.store, serde_json::json!({"claude": ["claude-haiku-4-5"]})).await;
    // Refuses a bearer, answers `x-api-key` — which is the whole difference
    // between the two dialects at the door.
    let base = models_stub_keyed_by("x-api-key", &["claude-haiku-4-5"]).await;

    // No protocol named: the probe has to work out that a bearer is refused
    // here and that this host is an Anthropic one.
    let r = commands::connect_custom_endpoint(
        &state,
        base,
        "sk-endpoint-key".into(),
        None,
        Some("relay".into()),
        Some(10),
        Some(4),
        None,
    )
    .await
    .expect("connect");
    assert_eq!(r["wire"], "claude", "found by asking, not by assuming");

    // And it travels: the gateway reads this field to decide which body to
    // build for the lane, so a lane that fails to carry it is one that gets
    // served OpenAI JSON by a host that speaks Anthropic.
    let items = asale_daemon::publisher::build_supply_items(&state.store, &state.pool).await;
    let haiku = items
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["model"] == "claude-haiku-4-5")
        .expect("the lane is declared")
        .clone();
    assert_eq!(haiku["wire"], "claude");

    let listed = commands::list_custom_endpoints(&state).await.unwrap();
    assert_eq!(listed["endpoints"][0]["wire"], "claude");
}

#[tokio::test(flavor = "current_thread")]
async fn a_protocol_this_client_cannot_speak_is_refused_rather_than_defaulted() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("custom-wire-unknown");
    let state = signed_in_state().await;
    as_operator(&state, Some(true)).await;

    // Silently demoting this to the default would connect the account, sell it,
    // and answer every request in a dialect the endpoint never agreed to. The
    // endpoint is not even probed on the way to the refusal.
    let e = commands::connect_custom_endpoint(
        &state,
        "https://example.invalid/v1".into(),
        "sk-x".into(),
        Some("bedrock".into()),
        None,
        None,
        None,
        None,
    )
    .await
    .expect_err("must be refused");
    assert!(e.message.contains("unknown endpoint protocol"), "got: {}", e.message);
}

#[tokio::test(flavor = "current_thread")]
async fn a_family_this_login_was_not_granted_is_refused_before_it_is_probed() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("custom-off");
    let state = signed_in_state().await;
    // An ordinary seller: the server answered, and it did not grant this
    // family. Knowing the RPC name is not enough, and the endpoint is never
    // probed on the way to the refusal.
    as_operator(&state, Some(false)).await;

    let e = commands::connect_custom_endpoint(
        &state,
        "https://example.invalid/v1".into(),
        "sk-x".into(),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect_err("must be refused");
    assert!(e.message.contains("not been granted"), "got: {}", e.message);

    // Same answer for the three pasted-key families, which used to be gated by
    // a flag compiled into the client and by nothing on the server at all.
    for provider in ["qwen", "deepseek", "openrouter"] {
        let e = commands::connect_api_key(&state, provider.into(), "sk-x".into(), None)
            .await
            .expect_err("must be refused");
        assert!(e.message.contains("not been granted"), "{provider}: {}", e.message);
    }

    // And an answer nobody gave is not a grant either.
    as_operator(&state, None).await;
    assert!(commands::connect_api_key(&state, "openrouter".into(), "sk-x".into(), None)
        .await
        .is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn an_endpoint_can_be_re_read_switched_and_removed() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("custom-manage");
    let state = signed_in_state().await;
    as_operator(&state, Some(true)).await;
    seed_catalog(&state.store, serde_json::json!({"claude": ["claude-haiku-4-5"]})).await;
    let base = models_stub(&["anthropic/claude-haiku-4.5"]).await;

    // The connect screen asks this before it draws anything.
    let offer = commands::connect_offer(&state).await.unwrap();
    let providers = offer["providers"].as_array().unwrap();
    assert!(providers.iter().any(|p| p == "custom"), "an operator is offered every family");
    assert!(providers.iter().any(|p| p == "claude"), "and still the ordinary ones");

    commands::connect_custom_endpoint(
        &state,
        base,
        "sk-endpoint-key".into(),
        None,
        Some("house".into()),
        Some(60),
        Some(8),
        None,
    )
    .await
    .expect("connect");

    // Listed with its terms and what it is actually selling.
    let listed = commands::list_custom_endpoints(&state).await.unwrap();
    let row = &listed["endpoints"][0];
    assert_eq!(row["account_id"], "house");
    assert_eq!(row["concurrency"], 8);
    assert_eq!(row["min_ratio"], 60);
    assert_eq!(row["sellable_models"][0], "claude-haiku-4-5");

    // Re-reading the model list is the button next to an endpoint whose
    // operator just added a model upstream.
    let r = commands::refresh_custom_endpoint(&state, "house".into()).await.expect("refresh");
    assert_eq!(r["endpoint_models"], 1);
    assert_eq!(r["sellable_models"][0], "claude-haiku-4-5");

    // The switch is the ordinary per-account one, and an endpoint that is off
    // keeps its terms rather than losing them.
    commands::set_account_sell(
        &state, "custom".into(), "house".into(), false, None, None, None, None, None,
    )
    .await
    .unwrap();
    let listed = commands::list_custom_endpoints(&state).await.unwrap();
    assert_eq!(listed["endpoints"][0]["sell_enabled"], false);
    assert_eq!(listed["endpoints"][0]["concurrency"], 8, "terms survive the switch");
    // Off the market entirely: nothing of this endpoint is declared. (Unlike a
    // withheld lane, which stays in the snapshot with a reason — a switch that
    // is off is not on the market at all.)
    let items = asale_daemon::publisher::build_supply_items(&state.store, &state.pool).await;
    assert!(items.as_array().unwrap().is_empty(), "a switched-off endpoint declares nothing");

    // Removal takes the row, the key and the two settings only this kind of
    // account has — a re-added endpoint must not inherit a stale model list.
    assert!(commands::remove_custom_endpoint(&state, "house".into()).await.unwrap());
    let listed = commands::list_custom_endpoints(&state).await.unwrap();
    assert!(listed["endpoints"].as_array().unwrap().is_empty());
    assert_eq!(
        state.store.get_setting("custombase:house").await.unwrap().unwrap_or_default(),
        "",
        "the endpoint URL went with it"
    );

}

#[tokio::test(flavor = "current_thread")]
async fn the_connect_offer_answers_either_way() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("custom-status");
    let state = signed_in_state().await;

    as_operator(&state, Some(true)).await;
    let offer = commands::connect_offer(&state).await.expect("always answerable");
    assert_eq!(offer["answered"], true);
    assert!(offer["providers"].as_array().unwrap().iter().any(|p| p == "custom"));

    // Answerable when nothing has been granted too — that is the whole point:
    // the connect grid has to know what to draw, and a refusal would be
    // indistinguishable from a daemon too old to know the command.
    as_operator(&state, Some(false)).await;
    let offer = commands::connect_offer(&state).await.expect("always answerable");
    assert_eq!(offer["answered"], true);
    assert!(!offer["providers"].as_array().unwrap().iter().any(|p| p == "custom"));
    assert!(offer["providers"].as_array().unwrap().iter().any(|p| p == "claude"));

    // And when the server said nothing at all, the compiled default — the set
    // that is right for everyone — rather than an empty screen.
    *state.capabilities.write().await = None;
    let offer = commands::connect_offer(&state).await.expect("always answerable");
    assert_eq!(offer["answered"], false, "a fallback must announce itself as one");
    let drawn = offer["providers"].as_array().unwrap();
    assert!(drawn.iter().any(|p| p == "claude"), "a stock client still offers the subscriptions");
    for granted in ["custom", "qwen", "deepseek", "openrouter"] {
        assert!(!drawn.iter().any(|p| p == granted), "`{granted}` is never drawn unasked");
    }
}

/// A family a stock build does not draw has to be granted before it can be
/// connected.
///
/// The gateway is what enforces it — `wsrelay::session::declare_supply` drops
/// the lanes from anybody else — and this is the client half: the tile is not
/// offered, and the command behind it refuses whether or not the caller found
/// the RPC name.
#[tokio::test(flavor = "current_thread")]
async fn an_ordinary_seller_is_not_offered_the_granted_families() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("custom-not-admin");
    let state = signed_in_state().await;
    as_operator(&state, Some(false)).await;

    let offer = commands::connect_offer(&state).await.expect("always answerable");
    let drawn = offer["providers"].as_array().unwrap();
    for granted in ["custom", "qwen", "deepseek", "openrouter"] {
        assert!(!drawn.iter().any(|p| p == granted), "`{granted}` must not be drawn");
    }

    // Not probed on the way to the refusal: the URL is unreachable and the
    // command must still fail on the grant rather than on a timeout.
    let e = commands::connect_custom_endpoint(
        &state,
        "https://example.invalid/v1".into(),
        "sk-x".into(),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect_err("must be refused");
    assert!(e.message.contains("not been granted"), "got: {}", e.message);

    // An unanswerable question is not a grant either — the daemon may simply
    // not have reached the server yet.
    as_operator(&state, None).await;
    let offer = commands::connect_offer(&state).await.unwrap();
    assert_eq!(offer["answered"], false);
}

/// The capability was open to everyone once, and a granted family can be taken
/// back, so an account may still hold one from before. It is cleared out rather
/// than left to sit on the sell page declaring supply the gateway refuses every
/// minute.
#[tokio::test(flavor = "current_thread")]
async fn an_ordinary_sellers_leftover_endpoint_is_cleared_out() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("custom-purge");
    let state = signed_in_state().await;
    seed_catalog(&state.store, serde_json::json!({"claude": ["claude-haiku-4-5"]})).await;
    let base = models_stub(&["anthropic/claude-haiku-4.5"]).await;

    // Connected back when it was allowed.
    as_operator(&state, Some(true)).await;
    commands::connect_custom_endpoint(
        &state,
        base,
        "sk-endpoint-key".into(),
        None,
        Some("house".into()),
        Some(5),
        Some(4),
        None,
    )
    .await
    .expect("connect");
    let items = asale_daemon::publisher::build_supply_items(&state.store, &state.pool).await;
    assert!(!items.as_array().unwrap().is_empty(), "it was on the market");

    // An unknown verdict is not a demotion — that is what a flight with no wifi
    // looks like, and the account it would delete holds a key the user pasted in
    // by hand. Driven through the supervisor's own entry point, because that is
    // where the guard has to hold.
    as_operator(&state, None).await;
    assert_eq!(
        commands::enforce_provider_policy(&state).await,
        0,
        "silence is not a demotion"
    );

    // Neither is a stale answer about whoever was signed in before. The verdict
    // is cached, nothing keys it to an account, and the one that survived a
    // sign-out would be deleting the *next* account's endpoints.
    as_operator(&state, Some(false)).await;
    commands::logout(&state).await.expect("logout");
    assert_eq!(
        commands::enforce_provider_policy(&state).await,
        0,
        "the previous account's verdict left with it"
    );

    // The server answering "no" about this one is.
    as_operator(&state, Some(false)).await;
    assert_eq!(commands::enforce_provider_policy(&state).await, 1);

    // Nothing left to declare, and nothing left for a re-added endpoint of the
    // same name to inherit.
    let items = asale_daemon::publisher::build_supply_items(&state.store, &state.pool).await;
    assert!(items.as_array().unwrap().is_empty(), "off the market");
    let tools = state.store.list_tools().await.unwrap();
    assert!(tools.iter().all(|t| t.provider != "custom"), "the account row went with it");
    assert_eq!(
        state.store.get_setting("custombase:house").await.unwrap().unwrap_or_default(),
        "",
        "the endpoint URL went with it"
    );

    // And it stays gone: the daemon will not re-offer the form.
    let offer = commands::connect_offer(&state).await.unwrap();
    assert!(!offer["providers"].as_array().unwrap().iter().any(|p| p == "custom"));
}

/// The declaration carries the floor the market prices against, and when two
/// accounts serve the same model it carries the cheaper of them.
///
/// The gateway prices a minute with no buyers at the best ask. Before this
/// field there was no ask to read, so it priced such a minute at `ratio_min` —
/// under every seller's floor — which withdrew them, which emptied the market,
/// which walked the price back up, which brought them back. One publisher was
/// enough to keep that running, and every lap re-listed and de-listed its whole
/// catalogue.
#[tokio::test(flavor = "current_thread")]
async fn the_declaration_carries_the_cheapest_floor_of_the_serving_accounts() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let _sb = Sandbox::new("ask");
    let state = signed_in_state().await;

    for (account, min_ratio) in [("dear@x.com", 60), ("cheap@x.com", 40)] {
        keychain::set(&keychain::token_ref("claude", account), "tok").unwrap();
        state
            .store
            .upsert_tool("claude", account, &keychain::token_ref("claude", account), &["test"], "oauth")
            .await
            .unwrap();
        state.store.set_setting(&format!("plan:claude:{account}"), "max_20x").await.unwrap();
        commands::set_account_sell(
            &state,
            "claude".into(),
            account.into(),
            true,
            None,
            Some(min_ratio),
            Some(100),
            None,
            None,
        )
        .await
        .unwrap();
    }

    let items = asale_daemon::publisher::build_supply_items(&state.store, &state.pool).await;
    let opus = items
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["model"] == "claude-opus-5")
        .expect("opus is declared");
    assert_eq!(
        opus["ask_ratio"], 40,
        "a buyer meeting the cheaper floor gets served by that account, so that is the ask"
    );
}
