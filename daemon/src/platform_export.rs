//! Explicit offline migration export. Never prints credentials or sends them over a network.
use crate::{keychain, publisher, state};
use asale_client_core::store::LocalStore;
use serde_json::json;
use std::io::Write;
pub async fn export(path: &str) -> anyhow::Result<usize> {
    let store = LocalStore::open(&format!("{}/asale.db", state::data_dir())).await?;
    let mut entries = Vec::new();
    for tool in store.list_tools().await? {
        let Some(spec) = asale_protocol::PROVIDERS
            .iter()
            .find(|p| p.id == tool.provider && !p.offered_by_default)
        else {
            continue;
        };
        let Some(credential) = keychain::get(&tool.keychain_ref)? else {
            continue;
        };
        let base = if tool.provider == "custom" {
            store
                .get_setting(&publisher::custom_base_key(&tool.account_id))
                .await?
                .unwrap_or_default()
        } else {
            spec.api_base.to_string()
        };
        let wire = if tool.provider == "custom" {
            store
                .get_setting(&publisher::custom_wire_key(&tool.account_id))
                .await?
                .unwrap_or_else(|| "openai".into())
        } else {
            "openai".into()
        };
        let listing = store
            .get_setting(&publisher::custom_models_key(
                &tool.provider,
                &tool.account_id,
            ))
            .await?
            .and_then(|s| serde_json::from_str::<publisher::CustomListing>(&s).ok())
            .unwrap_or_default();
        let models:Vec<_>=listing.aliases.into_iter().map(|(model,upstream_model)|json!({"model":model,"upstream_model":upstream_model,"sale":{},"cost":{}})).collect();
        entries.push(json!({"name":format!("{} / {}",tool.provider,tool.account_id),"provider":tool.provider,"credential":credential,"config":{"base_url":base,"wire":wire,"models":models,"visibility":"internal"}}));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(&serde_json::to_vec_pretty(
        &json!({"version":1,"entries":entries}),
    )?)?;
    file.sync_all()?;
    Ok(entries.len())
}
