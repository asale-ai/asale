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
async fn signed_in_state() -> AppState {
    let state = AppState::new().await.expect("app state");
    keychain::set("access_token", "test-access-token").unwrap();
    state.store.set_setting("asale_api_key", "sk-asale-test").await.unwrap();
    state
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
    commands::set_account_sell(&state, "claude".into(), "a@x.com".into(), true, None).await.unwrap();
    assert!(commands::publish_wanted(&state).await);
    assert_eq!(commands::proxy_status(&state).await.unwrap()["publish_wanted"], true);
    assert_eq!(
        commands::client_status(&state).await.unwrap()["selling"].as_array().unwrap().len(),
        1
    );

    // Switching the last one off takes the device back off the market, session
    // included — a device with nothing to sell must not hold a live session.
    commands::set_account_sell(&state, "claude".into(), "a@x.com".into(), false, None).await.unwrap();
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
    commands::set_account_sell(&state, "claude".into(), "owned@x.com".into(), true, Some(500_000))
        .await
        .unwrap();
    commands::set_account_sell(&state, "claude".into(), "shared@x.com".into(), false, None)
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
    commands::set_account_sell(&state, "claude".into(), "me@x.com".into(), true, Some(500_000))
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
    let off = commands::set_buy_tool(&state, "claude".into(), false, None).await.expect("buy off");
    assert_eq!(off["accounts"][0]["account_id"], "me@x.com", "sellable again straight away");
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
        commands::set_account_sell(&state, "claude".into(), account.into(), true, Some(1_000)).await.unwrap();
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
        .pick_for_sale("claude", "claude-opus-5", now)
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
    let state = AppState::new().await.unwrap();

    assert!(commands::set_buy_tool(&state, "emacs".into(), true, None).await.is_err());
    // Not signed in: must refuse before rewriting any config file.
    let path = tool_config::primary_config_path("claude");
    assert!(commands::set_buy_tool(&state, "claude".into(), true, None).await.is_err());
    assert!(!path.exists(), "a refused buy-on never creates a config file");
    // Unknown account.
    assert!(commands::set_account_sell(&state, "claude".into(), "nobody@x".into(), true, None)
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
    commands::set_account_sell(&state, "claude".into(), account.into(), true, None).await.unwrap();

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
            pool.pick_for_sale("claude", opus, now + i * 1_000).unwrap();
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
