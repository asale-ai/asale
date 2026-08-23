//! Browser OAuth, both flavours, as a two-step flow: begin returns
//! `{flow_id, auth_url}`, the frontend opens it, then polls `oauth_result`
//! until the loopback callback and token exchange finish.

use crate::keychain;
use crate::oauth;
use crate::publisher;
use crate::state::{AppState, FlowStatus};
use asale_client_core::{cli_import, device_flow, Provider};
use serde_json::{json, Value};
use std::sync::Arc;
use super::sell::{accounts_changed, credential_replaced};
use super::server_client::{finish_auth, resp_json};
use super::{CmdError, R, err, err_keyed, now_secs};
use crate::cmd_err;

/// Record a flow result.
pub(crate) async fn flow_set(state: &AppState, id: &str, status: FlowStatus) {
    state.oauth_flows.write().await.insert(id.to_string(), status);
}

/// Poll an in-flight OAuth flow. Terminal results are consumed (removed).
pub async fn oauth_result(state: &AppState, flow_id: String) -> R<Value> {
    let mut flows = state.oauth_flows.write().await;
    match flows.get(&flow_id) {
        None => Err(cmd_err!("errors.oauth.unknownFlow", "unknown flow")),
        Some(FlowStatus::Pending) => Ok(json!({ "status": "pending" })),
        Some(FlowStatus::Done(v)) => {
            let v = v.clone();
            flows.remove(&flow_id);
            Ok(json!({ "status": "ok", "result": v }))
        }
        Some(FlowStatus::Failed(e)) => {
            // Shaped like the RPC failure envelope (`error` + `key` + `params`)
            // so the frontend translates a flow that failed asynchronously the
            // same way it translates one that failed inline.
            let mut out = e.to_json();
            out["status"] = json!("error");
            flows.remove(&flow_id);
            Ok(out)
        }
    }
}

/// Open `url` in the machine-local browser, best-effort — only meaningful when
/// the daemon runs on a machine with a desktop (the Tauri shell case, or a
/// local browser session). Headless boxes simply skip.
pub(crate) fn open_local_browser(url: &str) {
    #[cfg(target_os = "linux")]
    {
        // On headless Linux there is no display server — don't block on xdg-open.
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return;
        }
    }
    let _ = open::that_detached(url);
}

/// Begin an OAuth login for a provider. Starts the loopback callback listener,
/// returns `{flow_id, auth_url}` right away, and finishes the exchange in the
/// background; the frontend polls `oauth_result`. `open_local=true` also opens
/// the URL in the daemon machine's browser (used by the desktop shell).
///
/// Note (remote B/S): the callback listens on this machine's loopback, which a
/// browser on another machine cannot reach — its `localhost` is its own. That
/// case finishes through `oauth_submit_code` (the user pastes the redirect URL
/// back), or through an SSH port-forward, or `import_from_cli`.
pub async fn oauth_login(state: &Arc<AppState>, provider: String, open_local: bool) -> R<Value> {
    let p = oauth::provider(&provider).ok_or_else(|| {
        if oauth::provider_unconfigured(&provider) {
            cmd_err!(
                "errors.oauth.providerUnconfigured",
                format!("{provider} sign-in is not configured in this build"),
                provider = provider.as_str()
            )
        } else {
            cmd_err!("errors.oauth.unknownProvider", "unknown provider", provider = provider.as_str())
        }
    })?;
    let (url, fut) = oauth::begin(&p).await.map_err(err)?;
    let flow_id = uuid::Uuid::new_v4().simple().to_string();
    flow_set(state, &flow_id, FlowStatus::Pending).await;
    // Taken before the future is moved into the task below; this is what
    // `oauth_submit_code` needs when the callback cannot be reached.
    state.oauth_submitters.write().await.insert(flow_id.clone(), fut.submitter());

    if open_local {
        open_local_browser(&url);
    }

    let st = state.clone();
    let fid = flow_id.clone();
    let prov = provider.clone();
    tokio::spawn(async move {
        let outcome = finish_provider_oauth(&st, &p, &prov, fut).await;
        st.oauth_submitters.write().await.remove(&fid);
        match outcome {
            Ok(v) => flow_set(&st, &fid, FlowStatus::Done(v)).await,
            Err(e) => flow_set(&st, &fid, FlowStatus::Failed(e)).await,
        }
    });

    Ok(json!({ "flow_id": flow_id, "auth_url": url, "provider": provider }))
}

/// Finish a login with a code the user pasted, for when the callback cannot
/// reach this machine.
///
/// The redirect goes to `http://localhost:<port>/callback`, and when the UI is
/// a browser on another machine that `localhost` is *that* machine — the page
/// fails to load and the code sits in an address bar the daemon will never see.
/// So the user copies it back here. `input` may be the whole URL, its query, or
/// the bare code; see [`oauth::extract_pasted_code`].
///
/// The flow then completes exactly as if the callback had fired: same token
/// exchange, same polling, same result. The frontend keeps polling
/// `oauth_result` either way.
pub async fn oauth_submit_code(state: &Arc<AppState>, flow_id: String, input: String) -> R<Value> {
    let Some(code) = oauth::extract_pasted_code(&input) else {
        return Err(cmd_err!(
            "errors.oauth.noCodeInPaste",
            "no authorization code in what was pasted"
        ));
    };
    let submitter = state.oauth_submitters.read().await.get(&flow_id).cloned();
    let Some(submitter) = submitter else {
        return Err(cmd_err!("errors.oauth.unknownFlow", "unknown flow"));
    };
    // False means the flow ended between the lookup and now — the callback got
    // through after all, or it timed out. Either way there is nothing to feed.
    if !submitter.submit(code) {
        return Err(cmd_err!("errors.oauth.unknownFlow", "unknown flow"));
    }
    Ok(json!({ "ok": true }))
}

/// Wait for the provider callback, exchange the code, persist tokens + tool row.
pub(crate) async fn finish_provider_oauth(
    state: &AppState,
    p: &oauth::OAuthProvider,
    provider: &str,
    fut: oauth::AuthCodeFuture,
) -> R<Value> {
    let code = tokio::time::timeout(std::time::Duration::from_secs(300), fut.wait())
        .await
        .map_err(|_| "authorization timed out".to_string())?
        .map_err(err)?;
    let tokens = oauth::exchange(p, &code).await.map_err(err)?;
    let access = tokens["access_token"].as_str().unwrap_or_default();
    if access.is_empty() {
        // Provider token responses may still contain a refresh token or an ID
        // token when they are malformed. Never echo the response into the RPC
        // error: that error is rendered by the frontend and may be collected by
        // browser diagnostics.
        return Err(missing_access_token_error(provider, &tokens));
    }
    let account_id = account_email(&tokens).unwrap_or_else(|| {
        // Key names only: every value in a token response is a secret.
        tracing::warn!(
            provider,
            "no email in the token response — falling back to a shared account id. \
             response keys = {:?}, account keys = {:?}",
            key_names(&tokens),
            key_names(&tokens["account"])
        );
        "account".to_string()
    });
    // Anthropic's exchange has never carried a plan, so a Claude login that
    // stopped here sold like the lowest paid tier — 220k tokens per five hours
    // against a Max 20×'s 4.4M. `oauth/profile` states it, costs no quota, and
    // is asked here so the very first declaration is already the right size
    // rather than waiting for the quota poll's next sweep.
    let plan = match account_plan(&tokens) {
        Some(p) => Some(p),
        None => claude_plan_from_profile_call(provider, access).await,
    };
    if plan.is_none() {
        tracing::warn!(
            provider,
            account_id,
            "no plan in the token response and none from the provider's profile — \
             quota will be estimated at the lowest tier. \
             response keys = {:?}, account keys = {:?}",
            key_names(&tokens),
            key_names(&tokens["account"])
        );
    }

    // Persist tokens in the secret store; the store only holds references (§3.4).
    keychain::set(&keychain::token_ref(provider, &account_id), access).map_err(err)?;
    if let Some(refresh) = tokens["refresh_token"].as_str() {
        keychain::set(&keychain::refresh_ref(provider, &account_id), refresh).map_err(err)?;
    }
    if let Some(secs) = tokens["expires_in"].as_i64() {
        let exp = (now_secs() + secs).to_string();
        state.store.set_setting(&publisher::exp_key(provider, &account_id), &exp).await.map_err(err)?;
    }
    // origin=oauth: asale ran this login itself, so it exclusively owns the
    // resulting refresh token — no rotation conflict with a local CLI (§4).
    state
        .store
        .upsert_tool(provider, &account_id, &keychain::token_ref(provider, &account_id), &["oauth"], "oauth")
        .await
        .map_err(err)?;
    if let Some(plan) = &plan {
        let _ = state.store.set_setting(&format!("plan:{provider}:{account_id}"), plan).await;
    }
    // Codex's upstream will not accept this bearer without the ChatGPT account
    // id issued with it, and asale's own login is the only place that id passes
    // through — the CLI import reads it out of auth.json, this path has to read
    // it out of the exchange. Missing it means every sale 401s.
    if let Some(up) = tokens["id_token"]
        .as_str()
        .and_then(cli_import::jwt_claims)
        .as_ref()
        .and_then(|c| c.get("https://api.openai.com/auth"))
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|a| a.as_str())
        .filter(|a| !a.is_empty())
    {
        let _ = state
            .store
            .set_setting(&publisher::upstream_acct_key(provider, &account_id), up)
            .await;
    }
    // Before the rebuild: a pause held on the credential this login just
    // replaced has no claim on the new one.
    credential_replaced(state, provider, &account_id).await;
    accounts_changed(state).await;

    Ok(json!({"provider": provider, "account_id": account_id, "keychain_ref": keychain::token_ref(provider, &account_id)}))
}

/// The key names of a JSON object, for saying what a response looked like
/// without saying what was in it — every value in a token response is a secret.
fn key_names(v: &Value) -> Vec<&str> {
    v.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default()
}

/// Which account this login belongs to, however the vendor spells it.
///
/// Anthropic answers `account.email_address`, others `account.email` or a bare
/// `email`, and Codex none of them — its identity is in the `id_token` claims.
/// Falling through all of them is not a cosmetic problem: the result becomes
/// `account_id`, which is the keychain key, so two accounts that both fall back
/// to the same placeholder overwrite each other's tokens.
fn account_email(tokens: &Value) -> Option<String> {
    tokens["account"]["email_address"]
        .as_str()
        .or_else(|| tokens["account"]["email"].as_str())
        .or_else(|| tokens["email"].as_str())
        .map(str::to_string)
        .or_else(|| oauth::id_token_claim(tokens, "email"))
        .filter(|email| !email.is_empty())
}

/// The subscription tier this account is on.
///
/// Anthropic names it `subscription_type` — the same word its CLI writes to
/// `.credentials.json` as `subscriptionType`, which is where the import path
/// already reads it. Codex puts it in the `id_token` instead, in the same
/// `https://api.openai.com/auth` block the caller reads the ChatGPT account id
/// from.
///
/// Also not cosmetic: `discovery::plan_window_cap` prices an unknown plan at the
/// lowest tier, so a Max subscription would advertise a Pro-sized window and go
/// dark after a handful of full-size sales, with matching answering `no_supply`.
fn account_plan(tokens: &Value) -> Option<String> {
    tokens["account"]["subscription_type"]
        .as_str()
        .or_else(|| tokens["account"]["plan"].as_str())
        .or_else(|| tokens["subscription_type"].as_str())
        .or_else(|| tokens["subscriptionType"].as_str())
        .or_else(|| tokens["plan"].as_str())
        .map(str::to_string)
        .or_else(|| codex_claim_plan(tokens))
        // Anthropic states the plan under `organization`, not as a
        // `subscription_type` anywhere. Usually only on the profile — but the
        // exchange's response has the same shape, and reading it here is one
        // fewer request on the logins where it is populated.
        .or_else(|| asale_client_core::discovery::claude_plan_from_profile(tokens))
        .filter(|plan| !plan.is_empty())
}

/// The plan from `oauth/profile`, for the providers that have one.
///
/// A failure is not an error to report: the login itself succeeded, the profile
/// is a refinement, and the quota poll will try again on its own clock. It is
/// logged at debug because the usual cause is a region-blocked upstream, which
/// this device already complains about everywhere else it matters.
async fn claude_plan_from_profile_call(provider: &str, access: &str) -> Option<String> {
    if !asale_protocol::ids::is_claude_family(provider) {
        return None;
    }
    match asale_client_core::discovery::fetch_claude_profile(access).await {
        Ok(body) => asale_client_core::discovery::claude_plan_from_profile(&body),
        Err(e) => {
            tracing::debug!(provider, "oauth/profile plan lookup failed: {e}");
            None
        }
    }
}

fn codex_claim_plan(tokens: &Value) -> Option<String> {
    oauth::id_token_claim(tokens, "chatgpt_plan_type").or_else(|| {
        tokens["id_token"]
            .as_str()
            .and_then(cli_import::jwt_claims)
            .as_ref()
            .and_then(|claims| claims.get("https://api.openai.com/auth"))
            .and_then(|auth| auth.get("chatgpt_plan_type"))
            .and_then(|plan| plan.as_str())
            .map(str::to_string)
    })
}

/// The provider's own account of why the exchange produced no token.
///
/// Vendors disagree on the shape: plain OAuth2 answers
/// `{"error", "error_description"}`, Anthropic `{"error": {"type", "message"}}`
/// and OpenAI `{"error": {"code", "message"}}`. Reading only the first two of
/// those flattened every other failure into a bare `missing_access_token`,
/// which is how a region block ends up indistinguishable from an expired
/// authorization code — with nothing in the log either way.
///
/// Only these two fields are read. The rest of the response may still hold a
/// refresh or ID token even when it is malformed, and this string is rendered
/// by the frontend.
fn provider_error_detail(tokens: &Value) -> String {
    let error = &tokens["error"];
    let code = error.as_str().or_else(|| error["code"].as_str()).or_else(|| error["type"].as_str());
    let text = tokens["error_description"].as_str().or_else(|| error["message"].as_str());
    match (code, text) {
        (Some(code), Some(text)) if code != text => format!("{code}: {text}"),
        (Some(code), _) => code.to_string(),
        (None, Some(text)) => text.to_string(),
        (None, None) => "missing_access_token".to_string(),
    }
}

fn missing_access_token_error(provider: &str, tokens: &Value) -> CmdError {
    let detail = provider_error_detail(tokens);
    // The RPC error is rendered in a webview; this is where the operator can
    // actually read it back after the dialog is gone.
    tracing::warn!(provider, "token exchange returned no access_token: {detail}");
    // `detail` is the name every `errors.oauth.*` entry interpolates. Sending
    // it under any other name renders the catalog's `{{detail}}` verbatim —
    // which is exactly what the sell page showed instead of this reason.
    cmd_err!(
        "errors.oauth.exchangeFailed",
        format!("token exchange failed ({detail})"),
        provider = provider,
        detail = detail
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_token_response_never_reaches_the_rpc_error() {
        let response = json!({
            "error": {"code": "invalid_grant"},
            "refresh_token": "refresh-secret",
            "id_token": "identity-secret",
            "client_secret": "client-secret"
        });
        let encoded = missing_access_token_error("codex", &response).to_json().to_string();
        assert!(encoded.contains("invalid_grant"));
        for secret in ["refresh-secret", "identity-secret", "client-secret"] {
            assert!(!encoded.contains(secret));
        }
    }

    /// Each vendor's own shape has to survive as something a user can act on —
    /// "this region is blocked" and "your code expired" are different problems.
    #[test]
    fn every_vendors_error_shape_keeps_its_reason() {
        let cases = [
            (json!({"error": {"type": "forbidden", "message": "Request not allowed"}}), "forbidden: Request not allowed"),
            (json!({"error": "invalid_grant", "error_description": "code expired"}), "invalid_grant: code expired"),
            (json!({"error": {"code": "invalid_client"}}), "invalid_client"),
            (json!({"error_description": "no client credential"}), "no client credential"),
            (json!({"token_type": "bearer"}), "missing_access_token"),
        ];
        for (response, want) in cases {
            assert_eq!(provider_error_detail(&response), want, "for {response}");
        }
    }

    /// Each vendor spells the identity differently, and none of them may end up
    /// on the shared `"account"` placeholder — that is the keychain key.
    #[test]
    fn every_vendors_identity_shape_is_recognized() {
        let cases = [
            (json!({"account": {"email_address": "a@x.com"}}), "a@x.com", "Anthropic"),
            (json!({"account": {"email": "b@x.com"}}), "b@x.com", "account.email"),
            (json!({"email": "c@x.com"}), "c@x.com", "bare email"),
        ];
        for (response, want, who) in cases {
            assert_eq!(account_email(&response).as_deref(), Some(want), "{who}");
        }
        assert_eq!(account_email(&json!({"account": {"email_address": ""}})), None, "blank is not an identity");
        assert_eq!(account_email(&json!({"token_type": "bearer"})), None);
    }

    #[test]
    fn every_vendors_plan_shape_is_recognized() {
        let cases = [
            (json!({"account": {"subscription_type": "max"}}), "max", "Anthropic"),
            (json!({"account": {"plan": "pro"}}), "pro", "account.plan"),
            (json!({"subscriptionType": "max"}), "max", "CLI spelling"),
            (json!({"plan": "team"}), "team", "bare plan"),
        ];
        for (response, want, who) in cases {
            assert_eq!(account_plan(&response).as_deref(), Some(want), "{who}");
        }
        assert_eq!(account_plan(&json!({"account": {"subscription_type": ""}})), None, "blank is not a plan");
        assert_eq!(account_plan(&json!({"token_type": "bearer"})), None);
    }

    /// The diagnostic that runs when the two above find nothing must not put the
    /// response itself in the log.
    #[test]
    fn the_shape_diagnostic_names_keys_and_nothing_else() {
        let response = json!({"access_token": "secret-a", "account": {"uuid": "secret-b"}});
        assert_eq!(key_names(&response), ["access_token", "account"]);
        assert_eq!(key_names(&response["account"]), ["uuid"]);
        assert!(key_names(&json!("not an object")).is_empty());
    }

    /// The catalog entry interpolates `detail`; anything else renders as
    /// literal `{{detail}}` in the UI.
    #[test]
    fn the_reason_travels_under_the_name_the_catalog_interpolates() {
        let response = json!({"error": {"type": "forbidden", "message": "Request not allowed"}});
        let encoded = missing_access_token_error("claude", &response).to_json();
        assert_eq!(encoded["key"], "errors.oauth.exchangeFailed");
        assert_eq!(encoded["params"]["detail"], "forbidden: Request not allowed");
    }
}

/// Begin a device-code login (Kimi Code, Grok CLI).
///
/// Same two-step contract as `oauth_login` — returns a `flow_id` the frontend
/// polls — but there is no loopback listener and therefore no requirement that
/// the browser run on this machine. That is what makes these two connectable
/// from a remote B/S session, where the authorization-code providers are not.
///
/// The extra fields are what the user needs on screen: `user_code` is the short
/// string they confirm, `auth_url` is where they confirm it (pre-filled with
/// the code when the provider offers that form).
pub async fn oauth_device_login(state: &Arc<AppState>, provider: String, open_local: bool) -> R<Value> {
    let p = Provider::from_str_opt(&provider)
        .filter(|p| asale_protocol::ids::is_device_flow_provider(*p))
        .ok_or_else(|| {
            cmd_err!(
                "errors.deviceFlow.unsupportedProvider",
                "this provider does not use a device-code login (kimi | xai)"
            )
        })?;

    let code = device_flow::begin(p).await.map_err(err_keyed)?;
    let flow_id = uuid::Uuid::new_v4().simple().to_string();
    flow_set(state, &flow_id, FlowStatus::Pending).await;

    // Read out what the user has to see before the polling task takes the code.
    let auth_url = code.verification_uri.clone();
    let user_code = code.user_code.clone();
    let expires_in = code.expires_in;

    if open_local {
        open_local_browser(&auth_url);
    }

    let st = state.clone();
    let fid = flow_id.clone();
    let prov = provider.clone();
    tokio::spawn(async move {
        let outcome = finish_device_login(&st, p, &prov, code).await;
        match outcome {
            Ok(v) => flow_set(&st, &fid, FlowStatus::Done(v)).await,
            Err(e) => flow_set(&st, &fid, FlowStatus::Failed(e)).await,
        }
    });

    Ok(json!({
        "flow_id": flow_id,
        "provider": provider,
        // `auth_url` keeps the same name the authorization-code flow uses, so
        // the frontend's existing open-and-poll helper drives both unchanged.
        "auth_url": auth_url,
        "user_code": user_code,
        "expires_in": expires_in,
    }))
}

/// Poll the token endpoint until the user approves, then persist exactly like
/// an authorization-code login: the credential is asale's own, so `origin` is
/// `oauth` and no shared-rotation warning applies.
async fn finish_device_login(
    state: &AppState,
    p: Provider,
    provider: &str,
    code: device_flow::DeviceCode,
) -> R<Value> {
    let tokens = device_flow::poll(p, &code).await.map_err(err_keyed)?;

    // An OIDC id_token names the account; Kimi issues none, so those accounts
    // fall back to a digest of the refresh token — stable across refreshes and
    // distinct per login, the same rule the CLI import uses.
    let account_id = tokens
        .id_token
        .as_deref()
        .and_then(cli_import::jwt_claims)
        .and_then(|c| {
            c.get("email")
                .or_else(|| c.get("sub"))
                .and_then(|e| e.as_str())
                .map(String::from)
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let cred = cli_import::CliCred {
                provider: provider.to_string(),
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                expires_at: tokens.expires_at,
                account_hint: None,
                plan: None,
                // Device-flow providers (Kimi) carry no vendor-side account id.
                upstream_account_id: None,
            };
            format!("{provider}-{}", cli_import::token_fingerprint(&cred))
        });

    keychain::set(&keychain::token_ref(provider, &account_id), &tokens.access_token).map_err(err)?;
    if let Some(refresh) = &tokens.refresh_token {
        keychain::set(&keychain::refresh_ref(provider, &account_id), refresh).map_err(err)?;
    }
    if let Some(exp) = tokens.expires_at {
        state
            .store
            .set_setting(&publisher::exp_key(provider, &account_id), &exp.to_string())
            .await
            .map_err(err)?;
    }
    state
        .store
        .upsert_tool(provider, &account_id, &keychain::token_ref(provider, &account_id), &["oauth"], "oauth")
        .await
        .map_err(err)?;
    credential_replaced(state, provider, &account_id).await;
    accounts_changed(state).await;

    Ok(json!({"provider": provider, "account_id": account_id}))
}

/// Sign in (or link) via a platform OAuth provider (google | github). PKCE +
/// loopback callback; the server builds the authorize URL and exchanges the
/// code. `link=false` logs in and stores tokens; `link=true` binds the
/// identity to the currently signed-in account. Two-step like `oauth_login`.
pub async fn platform_oauth_login(
    state: &Arc<AppState>,
    provider: String,
    link: bool,
    open_local: bool,
    region: String,
) -> R<Value> {
    if provider != "google" && provider != "github" {
        return Err(cmd_err!("errors.oauth.unknownProvider", "unknown provider", provider = provider.as_str()));
    }
    // Resolve the link token up front so we fail before opening the browser.
    let link_token = if link {
        Some(keychain::get("access_token").map_err(err)?.ok_or_else(|| cmd_err!("errors.session.notSignedIn", "not logged in"))?)
    } else {
        None
    };

    let pkce = oauth::gen_pkce();
    let cb = oauth::begin_platform_loopback().await.map_err(err)?;
    let redirect_uri = cb.redirect_uri.clone();
    let csrf = uuid::Uuid::new_v4().to_string();

    let http = asale_client_core::http::plain();
    let resp = http
        .get(format!("{}/api/v1/auth/oauth/{}/authorize", state.cfg.server_api_base, provider))
        .query(&[
            ("redirect_uri", redirect_uri.as_str()),
            ("state", csrf.as_str()),
            ("code_challenge", pkce.challenge.as_str()),
            // Picks the provider registration that accepts an ephemeral
            // loopback callback; the web app's cannot. Must be repeated on the
            // exchange, or the code goes to a different client_id.
            ("client", "desktop"),
        ])
        .send()
        .await
        .map_err(err)?;
    let v = resp_json(resp).await?;
    let auth_url = v["auth_url"].as_str().ok_or("server returned no auth_url")?.to_string();

    let flow_id = uuid::Uuid::new_v4().simple().to_string();
    flow_set(state, &flow_id, FlowStatus::Pending).await;
    if open_local {
        open_local_browser(&auth_url);
    }

    let st = state.clone();
    let fid = flow_id.clone();
    let prov = provider.clone();
    tokio::spawn(async move {
        let out: R<Value> = async {
            let code = cb.wait(&csrf, std::time::Duration::from_secs(300)).await.map_err(err)?;
            let http = asale_client_core::http::plain();
            let mut req = http
                .post(format!("{}/api/v1/auth/oauth/{}/exchange", st.cfg.server_api_base, prov))
                // `region` is the country the sign-up screen collected; the
                // server applies it only if this exchange creates an account,
                // so sending it on a returning user's login is a no-op.
                // `device_fp` for the same reason `auth::register` sends it:
                // the server's one-reward-per-device rule keys on it, and a
                // sign-up path that omits it registers no marks at all — which
                // reads downstream as "this machine already claimed" and
                // silently costs every desktop OAuth sign-up its welcome credit.
                .json(&json!({
                    "code": code,
                    "redirect_uri": redirect_uri,
                    "code_verifier": pkce.verifier,
                    "region": region,
                    "client": "desktop",
                    "device_fp": [st.device_id.clone()],
                }));
            if let Some(token) = link_token {
                req = req.header("authorization", format!("Bearer {token}"));
            }
            let v = resp_json(req.send().await.map_err(err)?).await?;
            if link {
                Ok(json!({"linked": v["linked"], "provider": v["provider"], "email": v["email"]}))
            } else {
                finish_auth(&st, &v).await
            }
        }
        .await;
        match out {
            Ok(v) => flow_set(&st, &fid, FlowStatus::Done(v)).await,
            Err(e) => flow_set(&st, &fid, FlowStatus::Failed(e)).await,
        }
    });

    Ok(json!({ "flow_id": flow_id, "auth_url": auth_url, "provider": provider }))
}
