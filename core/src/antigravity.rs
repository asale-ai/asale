//! Antigravity's OAuth control plane and inference envelope.
//! Protocol reference: CLIProxyAPI's Antigravity authenticator and executor.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

pub const CONTROL: &str = "https://cloudcode-pa.googleapis.com/v1internal";
pub const INFERENCE: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal";

pub fn user_agent() -> String {
    // Keep the version overridable without rebuilding when Google raises its floor.
    let version = std::env::var("ASALE_ANTIGRAVITY_VERSION")
        .ok()
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .unwrap_or_else(|| {
            option_env!("ASALE_ANTIGRAVITY_VERSION")
                .unwrap_or("2.23.0")
                .into()
        });
    format!("antigravity/hub/{version} darwin/arm64")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub project: String,
    pub models: Vec<String>,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub detail: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Antigravity returned {}: {}", self.status, self.detail)
    }
}
impl std::error::Error for ApiError {}

async fn call(
    http: &reqwest::Client,
    base: &str,
    method: &str,
    token: &str,
    body: Value,
) -> anyhow::Result<Value> {
    let resp = http
        .post(format!("{base}:{method}"))
        .bearer_auth(token)
        .header("user-agent", user_agent())
        .timeout(Duration::from_secs(30))
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        return Err(ApiError {
            status: status.as_u16(),
            detail: String::from_utf8_lossy(&bytes).into_owned(),
        }
        .into());
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        value.get("error").is_none(),
        "Antigravity {method} returned an error: {value}"
    );
    Ok(value)
}

fn project(value: &Value) -> Option<String> {
    ["cloudaicompanionProject", "projectId", "project"]
        .into_iter()
        .find_map(|key| {
            let v = &value[key];
            v.as_str()
                .or_else(|| v["id"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
}

fn rejection(value: &Value) -> Option<String> {
    let reasons: Vec<_> = value["ineligibleTiers"]
        .as_array()?
        .iter()
        .filter_map(|tier| {
            Some(format!(
                "{}: {}",
                tier["reasonCode"].as_str().unwrap_or("INELIGIBLE_TIER"),
                tier["reasonMessage"]
                    .as_str()
                    .unwrap_or("Account is not eligible")
            ))
        })
        .collect();
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

fn default_tier(loaded: &Value) -> &str {
    loaded["allowedTiers"]
        .as_array()
        .and_then(|tiers| tiers.iter().find(|t| t["isDefault"] == true))
        .and_then(|t| t["id"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            loaded["currentTier"]["id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("free-tier")
}

pub async fn resolve_project(http: &reqwest::Client, token: &str) -> anyhow::Result<String> {
    resolve_project_at(http, CONTROL, INFERENCE, token).await
}

async fn resolve_project_at(
    http: &reqwest::Client,
    control: &str,
    inference: &str,
    token: &str,
) -> anyhow::Result<String> {
    let loaded = call(
        http,
        control,
        "loadCodeAssist",
        token,
        json!({"metadata":{"ideType":"ANTIGRAVITY"}}),
    )
    .await?;
    if let Some(id) = project(&loaded) {
        return Ok(id);
    }
    let tier = loaded["allowedTiers"]
        .as_array()
        .and_then(|tiers| tiers.iter().find(|t| t["isDefault"] == true));
    // Never try to onboard a rejected account into an unrelated tier.
    if tier.is_none()
        && loaded["currentTier"]["id"]
            .as_str()
            .is_none_or(|id| id.trim().is_empty())
    {
        if let Some(reason) = rejection(&loaded) {
            anyhow::bail!("{reason}");
        }
    }
    let tier = default_tier(&loaded);
    let metadata = json!({"ide_type":"ANTIGRAVITY", "ide_name":"antigravity",
        "ide_version": user_agent().split('/').nth(2).unwrap_or("2.23.0").split(' ').next().unwrap_or("2.23.0")});
    for _ in 0..5 {
        let onboard = call(
            http,
            inference,
            "onboardUser",
            token,
            json!({"tier_id":tier, "metadata":metadata}),
        )
        .await?;
        if onboard["done"] == true {
            if let Some(error) = onboard.get("error") {
                anyhow::bail!("Antigravity onboarding failed: {error}");
            }
            return project(&onboard["response"])
                .or_else(|| project(&onboard))
                .ok_or_else(|| anyhow::anyhow!("Antigravity onboarding returned no project"));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    anyhow::bail!("Antigravity onboarding did not complete")
}

/// Both login and periodic publication use the account's actual model catalog.
pub async fn discover(http: &reqwest::Client, token: &str) -> anyhow::Result<Account> {
    discover_at(http, CONTROL, INFERENCE, token).await
}

async fn discover_at(
    http: &reqwest::Client,
    control: &str,
    inference: &str,
    token: &str,
) -> anyhow::Result<Account> {
    let project = resolve_project_at(http, control, inference, token).await?;
    let value = call(
        http,
        inference,
        "fetchAvailableModels",
        token,
        json!({"project":project}),
    )
    .await?;
    Ok(Account {
        project,
        models: models(&value)?,
    })
}

fn models(value: &Value) -> anyhow::Result<Vec<String>> {
    let map = value["models"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Antigravity returned no model catalog"))?;
    let mut ids: Vec<_> = map
        .keys()
        .map(|s| s.trim().trim_start_matches("models/").to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

pub fn url(stream: bool) -> String {
    format!(
        "{INFERENCE}:{}",
        if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        }
    )
}

pub fn request(body: &[u8], model: &str, project: &str, task_id: &str) -> anyhow::Result<Vec<u8>> {
    let mut inner: Value = serde_json::from_slice(body)?;
    let obj = inner
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Antigravity request must be an object"))?;
    obj.remove("model");
    obj.remove("stream");
    obj.remove("safetySettings");
    let claude = model.contains("claude");
    if let Some(tools) = obj.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if let Some(declarations) = tool
                .get_mut("functionDeclarations")
                .and_then(Value::as_array_mut)
            {
                for declaration in declarations {
                    let Some(fields) = declaration.as_object_mut() else {
                        continue;
                    };
                    let schema = fields
                        .remove("parametersJsonSchema")
                        .or_else(|| fields.remove("parameters"))
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({"type":"object"}));
                    fields.insert(
                        "parameters".into(),
                        tool_schema(&schema, &schema, claude, 0)?,
                    );
                }
            }
        }
        if claude
            && obj
                .get("toolConfig")
                .and_then(|v| v.pointer("/functionCallingConfig/mode"))
                .and_then(Value::as_str)
                .is_none_or(|mode| mode == "AUTO")
        {
            obj.insert(
                "toolConfig".into(),
                json!({"functionCallingConfig":{"mode":"VALIDATED"}}),
            );
        }
    }
    // Conversation content, rather than the task id, keeps retries on one session.
    if !obj.contains_key("sessionId") {
        use sha2::{Digest, Sha256};
        let first_turn = obj
            .get("contents")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or(Value::Null);
        let digest = Sha256::digest(serde_json::to_vec(&first_turn)?);
        // Match the reference client's signed 64-bit decimal session identifier.
        let number =
            u64::from_be_bytes(digest[..8].try_into().expect("eight bytes")) & i64::MAX as u64;
        let session = format!("-{number}");
        obj.insert("sessionId".into(), json!(session));
    }
    Ok(serde_json::to_vec(
        &json!({"model":model.trim_start_matches("models/"),
        "project":project, "userAgent":"antigravity", "requestType":"agent",
        "requestId":format!("agent-{task_id}"), "request":inner}),
    )?)
}

/// Translate schema nodes only. Keys in conversation arguments and property
/// names (including "default", "type", etc.) must never be treated as keywords.
/// Like CLIProxyAPI, retain unsupported constraints as description hints.
fn tool_schema(value: &Value, root: &Value, claude: bool, depth: usize) -> anyhow::Result<Value> {
    anyhow::ensure!(
        depth < 48,
        "Antigravity tool schema is recursive or too deeply nested"
    );
    let mut node = value.as_object().cloned().unwrap_or_default();
    if let Some(reference) = node.remove("$ref") {
        let pointer = reference
            .as_str()
            .and_then(|r| r.strip_prefix('#'))
            .ok_or_else(|| anyhow::anyhow!("Antigravity tools require local schema references"))?;
        let target = root.pointer(pointer).ok_or_else(|| {
            anyhow::anyhow!("Antigravity tool schema has an unresolved reference")
        })?;
        let mut resolved = tool_schema(target, root, claude, depth + 1)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        resolved.extend(node);
        node = resolved;
    }
    let mut hints = Vec::new();
    for key in ["allOf", "anyOf", "oneOf"] {
        let Some(branches) = node.remove(key).and_then(|v| v.as_array().cloned()) else {
            continue;
        };
        let nullable = branches.iter().any(|v| v["type"] == "null");
        let mut branches: Vec<Value> = branches
            .into_iter()
            .filter(|v| v["type"] != "null")
            .map(|v| tool_schema(&v, root, claude, depth + 1))
            .collect::<anyhow::Result<_>>()?;
        if nullable {
            node.insert("nullable".into(), json!(true));
        }
        if key != "allOf" && branches.len() > 1 && !claude {
            node.insert(key.into(), json!(branches));
            continue;
        }
        if key != "allOf" && branches.len() > 1 {
            hints.push(format!("{key}: {}", json!(branches)));
            // Claude's bridge rejects multi-branch tool unions. Prefer an
            // object branch, matching the reference's strongest-shape policy.
            branches.sort_by_key(|v| {
                std::cmp::Reverse(v["properties"].as_object().map_or(0, |p| p.len() + 1))
            });
            branches.truncate(1);
        }
        for branch in branches {
            if let Some(fields) = branch.as_object() {
                for (key, value) in fields {
                    if key == "properties" {
                        let props = node.entry(key.clone()).or_insert_with(|| json!({}));
                        if let (Some(dest), Some(src)) = (props.as_object_mut(), value.as_object())
                        {
                            dest.extend(src.clone());
                        }
                    } else if key == "required" {
                        let required = node.entry(key.clone()).or_insert_with(|| json!([]));
                        if let (Some(dest), Some(src)) = (required.as_array_mut(), value.as_array())
                        {
                            for v in src {
                                if !dest.contains(v) {
                                    dest.push(v.clone());
                                }
                            }
                        }
                    } else {
                        node.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
            }
        }
    }
    if let Some(types) = node.get("type").and_then(Value::as_array).cloned() {
        if types.iter().any(|t| t == "null") {
            node.insert("nullable".into(), json!(true));
        }
        node.insert(
            "type".into(),
            types
                .into_iter()
                .find(|t| t != "null")
                .unwrap_or(json!("string")),
        );
    }
    // A positive allow-list keeps unsupported protobuf fields off the wire.
    let keys: Vec<String> = node.keys().cloned().collect();
    for key in keys {
        if !matches!(
            key.as_str(),
            "type"
                | "properties"
                | "items"
                | "required"
                | "description"
                | "nullable"
                | "anyOf"
                | "oneOf"
        ) {
            let removed = node.remove(&key).unwrap();
            if !matches!(
                key.as_str(),
                "$defs" | "definitions" | "$schema" | "$id" | "title" | "$comment"
            ) {
                hints.push(format!("{key}: {removed}"));
            }
        }
    }
    if let Some(properties) = node.get_mut("properties").and_then(Value::as_object_mut) {
        for schema in properties.values_mut() {
            *schema = tool_schema(schema, root, claude, depth + 1)?;
        }
        node.insert("type".into(), json!("object"));
    }
    if node
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.eq_ignore_ascii_case("array"))
    {
        let items = node
            .remove("items")
            .unwrap_or_else(|| json!({"type":"string"}));
        node.insert(
            "items".into(),
            tool_schema(&items, root, claude, depth + 1)?,
        );
    }
    if let Some(required) = node.remove("required").and_then(|v| v.as_array().cloned()) {
        let valid: Vec<Value> = required
            .into_iter()
            .filter(|v| {
                v.as_str()
                    .is_some_and(|k| node.get("properties").and_then(|p| p.get(k)).is_some())
            })
            .collect();
        if !valid.is_empty() {
            node.insert("required".into(), json!(valid));
        }
    }
    if claude
        && node
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.eq_ignore_ascii_case("object"))
    {
        let props = node.entry("properties").or_insert_with(|| json!({}));
        if props.as_object().is_some_and(|p| p.is_empty()) {
            *props = json!({"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"}});
            node.insert("required".into(), json!(["reason"]));
        }
    }
    if !hints.is_empty() {
        let existing = node
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        node.insert(
            "description".into(),
            json!(format!("{existing}\n{}", hints.join("; ")).trim()),
        );
    }
    Ok(Value::Object(node))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onboarding_tier_matches_antigravity_not_legacy_gemini() {
        assert_eq!(default_tier(&json!({})), "free-tier");
        assert_eq!(
            default_tier(&json!({"currentTier":{"id":"paid-tier"}})),
            "paid-tier"
        );
        assert_eq!(
            default_tier(
                &json!({"allowedTiers":[{"id":"","isDefault":true}],"currentTier":{"id":"paid-tier"}})
            ),
            "paid-tier"
        );
        assert_eq!(
            default_tier(
                &json!({"allowedTiers":[{"id":"default-tier","isDefault":true}],"currentTier":{"id":"paid-tier"}})
            ),
            "default-tier"
        );
        assert!(
            rejection(&json!({"ineligibleTiers":[{"reasonCode":"UNSUPPORTED_CLIENT"}]}))
                .unwrap()
                .starts_with("UNSUPPORTED_CLIENT:")
        );
    }

    #[test]
    fn claude_tool_schemas_are_normalized_without_changing_history() {
        let history = json!([{"role":"model","parts":[{"functionCall":{"name":"run","args":{"default":42,"type":"literal","additionalProperties":false}}}]}]);
        let body = json!({"contents":history,"tools":[{"functionDeclarations":[{"name":"run","parametersJsonSchema":{
            "$defs":{"Arg":{"type":["string","null"],"enum":["a","b"]}},
            "type":"object","properties":{"default":{"$ref":"#/$defs/Arg"},"items":{"type":"array"}},
            "additionalProperties":false,"required":["default","missing"]
        }},{"name":"status"}]}]});
        let output: Value = serde_json::from_slice(
            &request(
                &serde_json::to_vec(&body).unwrap(),
                "claude-sonnet-4-5",
                "p",
                "t",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(output["request"]["contents"], history);
        assert_eq!(
            output["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
        let schema = &output["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(schema.get("additionalProperties").is_none());
        assert_eq!(schema["properties"]["default"]["type"], "string");
        assert_eq!(schema["properties"]["default"]["nullable"], true);
        assert!(schema["properties"]["default"]["description"]
            .as_str()
            .unwrap()
            .contains("enum"));
        assert_eq!(schema["properties"]["items"]["items"]["type"], "string");
        assert_eq!(schema["required"], json!(["default"]));
        assert_eq!(
            output["request"]["tools"][0]["functionDeclarations"][1]["parameters"]["required"],
            json!(["reason"])
        );
        let cyclic = json!({"$ref":"#"});
        assert!(tool_schema(&cyclic, &cyclic, true, 0).is_err());
    }

    #[test]
    fn envelope_preserves_tools_and_thought_signatures() {
        let input = br#"{"model":"ignored","stream":true,"contents":[{"role":"model","parts":[{"functionCall":{"name":"read","args":{}},"thoughtSignature":"upstream-signature"}]}],"tools":[{"functionDeclarations":[{"name":"read"}]}]}"#;
        let value: Value =
            serde_json::from_slice(&request(input, "models/claude-sonnet-4-5", "p", "t").unwrap())
                .unwrap();
        assert_eq!(value["userAgent"], "antigravity");
        assert_eq!(value["model"], "claude-sonnet-4-5");
        assert_eq!(
            value["request"]["contents"][0]["parts"][0]["thoughtSignature"],
            "upstream-signature"
        );
        assert!(value["request"].get("stream").is_none());
        assert_eq!(
            value["request"]["tools"][0]["functionDeclarations"][0]["name"],
            "read"
        );
        let retry: Value =
            serde_json::from_slice(&request(input, "claude-sonnet-4-5", "p", "retry").unwrap())
                .unwrap();
        assert_eq!(value["request"]["sessionId"], retry["request"]["sessionId"]);
        assert!(
            value["request"]["sessionId"]
                .as_str()
                .unwrap()
                .parse::<i64>()
                .unwrap()
                <= 0
        );
    }

    #[test]
    fn model_discovery_distinguishes_empty_from_malformed() {
        assert_eq!(
            models(&json!({"models":{"gemini-3.5-flash":{},"claude-sonnet-4-5":{}}})).unwrap(),
            vec!["claude-sonnet-4-5", "gemini-3.5-flash"]
        );
        assert!(models(&json!({"models":{}})).unwrap().is_empty());
        assert!(models(&json!({})).is_err());
    }

    #[tokio::test]
    async fn discovery_uses_the_managed_project_to_fetch_account_models() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for onboarding in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/v1internal", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let mut steps = vec![
                    (
                        "loadCodeAssist",
                        if onboarding {
                            json!({"currentTier":{"id":"paid-tier"},"ineligibleTiers":[{"reasonCode":"OTHER_TIER_UNAVAILABLE","reasonMessage":"not on the other plan"}]})
                        } else {
                            json!({"cloudaicompanionProject":{"id":"managed"}})
                        },
                    ),
                    (
                        "fetchAvailableModels",
                        json!({"models":{"gemini-3.5-flash":{},"claude-sonnet-4-5":{}}}),
                    ),
                ];
                if onboarding {
                    steps.insert(1, ("onboardUser", json!({"done":true,"response":{"cloudaicompanionProject":{"id":"managed"}}})));
                }
                for (method, body) in steps {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut input = Vec::new();
                    loop {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap();
                        assert!(n > 0);
                        input.extend_from_slice(&buf[..n]);
                        let raw = String::from_utf8_lossy(&input);
                        if let Some((headers, data)) = raw.split_once("\r\n\r\n") {
                            let length = headers
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if data.len() >= length {
                                break;
                            }
                        }
                    }
                    let raw = String::from_utf8(input).unwrap();
                    assert!(raw.starts_with(&format!("POST /v1internal:{method} ")));
                    assert!(raw
                        .to_ascii_lowercase()
                        .contains("authorization: bearer test"));
                    if method == "onboardUser" {
                        assert!(raw.contains(r#""tier_id":"paid-tier""#));
                        assert!(raw.contains(r#""ide_type":"ANTIGRAVITY""#));
                    }
                    if method == "fetchAvailableModels" {
                        assert!(raw.contains(r#""project":"managed""#));
                    }
                    let data = body.to_string();
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len()).as_bytes()).await.unwrap();
                }
            });
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let account = discover_at(&http, &base, &base, "test").await.unwrap();
            assert_eq!(account.project, "managed");
            assert_eq!(
                account.models,
                vec!["claude-sonnet-4-5", "gemini-3.5-flash"]
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejected_accounts_are_not_onboarded() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1internal", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.contains(":loadCodeAssist"));
            assert!(req.contains("ANTIGRAVITY"));
            let body = r#"{"ineligibleTiers":[{"reasonCode":"UNSUPPORTED_CLIENT","reasonMessage":"migrate"}]}"#;
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let error = resolve_project_at(
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            &base,
            &base,
            "test",
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "UNSUPPORTED_CLIENT: migrate");
        server.await.unwrap();
    }
}
