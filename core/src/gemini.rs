//! Gemini CLI subscriptions use Code Assist, not the API-key inference service.
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use std::{io, pin::Pin, time::Duration};

pub const CODE_ASSIST: &str = "https://cloudcode-pa.googleapis.com/v1internal";
pub fn user_agent(model: &str) -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    format!("GeminiCLI/0.62.0/{model} ({os}; {arch}; cli)")
}

fn project_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("id").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

async fn control(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    method: &str,
    body: Value,
) -> anyhow::Result<Value> {
    let resp = http
        .post(format!("{base}:{method}"))
        .bearer_auth(token)
        .header("user-agent", user_agent("gemini-2.5-pro"))
        .timeout(Duration::from_secs(30))
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let value: Value = resp.json().await?;
    anyhow::ensure!(
        status.is_success(),
        "Code Assist {method} returned {status}: {value}"
    );
    Ok(value)
}

/// Resolve the Google-managed project using the same control plane as Gemini CLI.
/// Kept account-local at the caller; never supplied by the relay gateway.
pub async fn resolve_project(http: &reqwest::Client, token: &str) -> anyhow::Result<String> {
    let project = ["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT_ID"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok().filter(|s| !s.trim().is_empty()));
    resolve_project_at(http, CODE_ASSIST, token, project.as_deref()).await
}

fn project_error(loaded: &Value) -> anyhow::Error {
    if let Some(tiers) = loaded["ineligibleTiers"].as_array() {
        let reasons: Vec<String> = tiers
            .iter()
            .filter_map(|tier| {
                let reason = tier["reasonMessage"].as_str()?;
                let code = tier["reasonCode"].as_str().unwrap_or("INELIGIBLE_TIER");
                Some(format!("{code}: {reason}"))
            })
            .collect();
        if !reasons.is_empty() {
            return anyhow::anyhow!("{}", reasons.join("; "));
        }
    }
    anyhow::anyhow!(
        "Code Assist requires a Google Cloud project; set GOOGLE_CLOUD_PROJECT for this account"
    )
}

async fn resolve_project_at(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    configured: Option<&str>,
) -> anyhow::Result<String> {
    let configured = configured.map(str::trim).filter(|s| !s.is_empty());
    if let Some(project) = configured {
        anyhow::ensure!(
            !project.chars().all(|c| c.is_ascii_digit()),
            "GOOGLE_CLOUD_PROJECT must be a project ID, not a project number"
        );
    }
    let mut metadata = json!({"ideType":"IDE_UNSPECIFIED", "platform":"PLATFORM_UNSPECIFIED", "pluginType":"GEMINI"});
    let mut load_body = json!({"metadata":metadata});
    if let Some(project) = configured {
        metadata["duetProject"] = json!(project);
        load_body["metadata"] = metadata.clone();
        load_body["cloudaicompanionProject"] = json!(project);
    }
    let loaded = control(http, base, token, "loadCodeAssist", load_body).await?;
    if let Some(project) = project_id(&loaded["cloudaicompanionProject"]) {
        return Ok(project);
    }
    if loaded["currentTier"].is_object() {
        return configured
            .map(str::to_owned)
            .ok_or_else(|| project_error(&loaded));
    }
    let tier = loaded["allowedTiers"]
        .as_array()
        .and_then(|tiers| tiers.iter().find(|t| t["isDefault"] == true));
    // Standard/legacy tiers need the operator's GCP project. Do not onboard
    // a rejected personal account into an unrelated empty standard project.
    if configured.is_none() && tier.is_some_and(|t| t["userDefinedCloudaicompanionProject"] == true)
    {
        return Err(project_error(&loaded));
    }
    let tier_id = tier.and_then(|t| t["id"].as_str()).unwrap_or("legacy-tier");
    let mut body = json!({"tierId":tier_id, "metadata":metadata});
    if tier_id != "free-tier" {
        if let Some(project) = configured {
            body["cloudaicompanionProject"] = json!(project);
        }
    } else {
        body["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("duetProject");
    }
    let mut onboard = control(http, base, token, "onboardUser", body).await?;
    for _ in 0..10 {
        if onboard["done"] == true {
            if let Some(error) = onboard.get("error") {
                anyhow::bail!("Code Assist onboarding failed: {error}");
            }
            return project_id(&onboard["response"]["cloudaicompanionProject"])
                .or_else(|| configured.map(str::to_owned))
                .ok_or_else(|| project_error(&loaded));
        }
        let name = onboard["name"]
            .as_str()
            .filter(|name| {
                name.starts_with("operations/")
                    && !name.contains("..")
                    && !name.contains(['?', '#'])
            })
            .ok_or_else(|| anyhow::anyhow!("Code Assist onboarding returned no valid operation"))?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let response = http
            .get(format!("{base}/{name}"))
            .bearer_auth(token)
            .header("user-agent", user_agent("gemini-2.5-pro"))
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        onboard = response.error_for_status()?.json().await?;
    }
    anyhow::bail!("Code Assist onboarding did not complete")
}

/// Keep the native Gemini request intact inside Code Assist's envelope.
pub fn request(body: &[u8], model: &str, project: &str, task_id: &str) -> anyhow::Result<Vec<u8>> {
    let mut inner: Value = serde_json::from_slice(body)?;
    let obj = inner
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Gemini request must be an object"))?;
    obj.remove("model");
    obj.remove("stream");
    Ok(serde_json::to_vec(
        &json!({"model":model.trim_start_matches("models/"), "project":project,
        "user_prompt_id":task_id, "request":inner}),
    )?)
}

pub fn url(stream: bool) -> String {
    format!(
        "{CODE_ASSIST}:{}",
        if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        }
    )
}

/// Restore the native Gemini response expected by translators and usage metering.
pub fn response(body: &[u8]) -> io::Result<Vec<u8>> {
    let envelope: Value = serde_json::from_slice(body)?;
    if envelope.get("error").is_some() {
        return Ok(body.to_vec());
    }
    let mut response = envelope
        .get("response")
        .filter(|v| v.is_object())
        .cloned()
        .ok_or_else(|| io::Error::other("Code Assist returned no response object"))?;
    if let Some(trace) = envelope.get("traceId") {
        response
            .as_object_mut()
            .unwrap()
            .entry("responseId")
            .or_insert_with(|| trace.clone());
    }
    Ok(serde_json::to_vec(&response)?)
}

#[derive(Default)]
struct SseDecoder {
    pending: Vec<u8>,
}
impl SseDecoder {
    fn line(line: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(data) = line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            out.extend_from_slice(b"data: ");
            if data == b"[DONE]" {
                out.extend_from_slice(data);
            } else {
                out.extend_from_slice(&response(data)?);
            }
        } else {
            out.extend_from_slice(line);
        }
        out.push(b'\n');
        Ok(())
    }
    fn push(&mut self, bytes: &[u8], eof: bool) -> io::Result<Vec<u8>> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut start = 0;
        for end in 0..self.pending.len() {
            if self.pending[end] == b'\n' {
                Self::line(&self.pending[start..end], &mut out)?;
                start = end + 1;
            }
        }
        self.pending.drain(..start);
        if eof && !self.pending.is_empty() {
            Self::line(&self.pending, &mut out)?;
            out.push(b'\n');
            self.pending.clear();
        }
        Ok(out)
    }
}

/// Normalize wrapped SSE only for Code Assist; other providers pass unchanged.
pub fn stream(
    resp: reqwest::Response,
    code_assist: bool,
) -> Pin<Box<dyn Stream<Item = io::Result<Vec<u8>>> + Send>> {
    let source = Box::pin(resp.bytes_stream());
    Box::pin(futures_util::stream::unfold(
        (source, SseDecoder::default(), false),
        move |(mut source, mut decoder, done)| async move {
            if done {
                return None;
            }
            loop {
                match source.next().await {
                    Some(Ok(bytes)) => {
                        let output = if code_assist {
                            decoder.push(&bytes, false)
                        } else {
                            Ok(bytes.to_vec())
                        };
                        match output {
                            Ok(bytes) if bytes.is_empty() => continue,
                            output => {
                                let failed = output.is_err();
                                return Some((output, (source, decoder, failed)));
                            }
                        }
                    }
                    Some(Err(e)) => {
                        return Some((Err(io::Error::other(e)), (source, decoder, true)))
                    }
                    None => {
                        let tail = decoder.push(&[], true);
                        return match tail {
                            Ok(bytes) if bytes.is_empty() => None,
                            result => Some((result, (source, decoder, true))),
                        };
                    }
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn control_server(
        steps: Vec<(&'static str, &'static str, Value)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1internal", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            for (method, path, body) in steps {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut input = Vec::new();
                loop {
                    let mut buf = [0; 1024];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0, "request ended before headers");
                    input.extend_from_slice(&buf[..n]);
                    if input.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&input);
                assert!(headers.starts_with(&format!("{method} {path} HTTP/1.1")));
                assert!(headers
                    .to_lowercase()
                    .contains("authorization: bearer test-token"));
                assert!(headers.contains("GeminiCLI/"));
                let body = serde_json::to_vec(&body).unwrap();
                let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        (base, task)
    }

    #[tokio::test]
    async fn existing_project_does_not_onboard_again() {
        let (base, server) = control_server(vec![(
            "POST",
            "/v1internal:loadCodeAssist",
            json!({"cloudaicompanionProject":"managed-project","currentTier":{"id":"free-tier"}}),
        )])
        .await;
        assert_eq!(
            resolve_project_at(&reqwest::Client::new(), &base, "test-token", None)
                .await
                .unwrap(),
            "managed-project"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn onboarding_polls_operation_instead_of_reposting() {
        let (base, server) = control_server(vec![
            (
                "POST",
                "/v1internal:loadCodeAssist",
                json!({"allowedTiers":[{"id":"free-tier","isDefault":true}]}),
            ),
            (
                "POST",
                "/v1internal:onboardUser",
                json!({"name":"operations/setup","done":false}),
            ),
            (
                "GET",
                "/v1internal/operations/setup",
                json!({"done":true,"response":{"cloudaicompanionProject":{"id":"new-project"}}}),
            ),
        ])
        .await;
        assert_eq!(
            resolve_project_at(&reqwest::Client::new(), &base, "test-token", None)
                .await
                .unwrap(),
            "new-project"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejected_personal_tier_preserves_actionable_reason_without_onboarding() {
        let (base, server) = control_server(vec![("POST", "/v1internal:loadCodeAssist", json!({
            "allowedTiers":[{"id":"standard-tier","isDefault":true,"userDefinedCloudaicompanionProject":true}],
            "ineligibleTiers":[{"reasonCode":"UNSUPPORTED_CLIENT","reasonMessage":"Migrate to Antigravity"}],
        }))]).await;
        let error = resolve_project_at(&reqwest::Client::new(), &base, "test-token", None)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "UNSUPPORTED_CLIENT: Migrate to Antigravity");
        assert!(!error.contains("allowedTiers"));
        server.await.unwrap();
    }

    #[test]
    fn wraps_native_request_without_losing_tools_or_thinking() {
        let body = json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}],
            "tools":[{"functionDeclarations":[{"name":"lookup"}]}],
            "generationConfig":{"thinkingConfig":{"thinkingBudget":1024}},"model":"wrong","stream":true});
        let result: Value = serde_json::from_slice(
            &request(
                &serde_json::to_vec(&body).unwrap(),
                "models/gemini-2.5-flash",
                "project",
                "task",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(result["model"], "gemini-2.5-flash");
        assert_eq!(result["project"], "project");
        assert_eq!(result["request"]["tools"], body["tools"]);
        assert_eq!(
            result["request"]["generationConfig"],
            body["generationConfig"]
        );
        assert!(result["request"].get("stream").is_none());
    }
    #[test]
    fn streamed_envelopes_survive_every_byte_boundary_and_meter_usage() {
        let native = json!({"candidates":[{"content":{"parts":[{"text":"你好","thought":false},{"functionCall":{"name":"lookup","args":{}}}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}});
        let input = format!(
            "data: {}\r\n\r\ndata: [DONE]\n\n",
            json!({"response":native,"traceId":"trace"})
        );
        let mut decoder = SseDecoder::default();
        let mut output = Vec::new();
        for b in input.as_bytes() {
            output.extend(decoder.push(&[*b], false).unwrap());
        }
        output.extend(decoder.push(&[], true).unwrap());
        let mut scanner = crate::executor::UsageScanner::new();
        scanner.push(&output);
        let usage = scanner.flush();
        assert_eq!((usage.input_tokens, usage.output_tokens), (7, 3));
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("你好"));
        assert!(text.contains("functionCall"));
        assert!(text.contains("responseId"));
        assert!(!text.contains("\"response\":"));
    }
    #[test]
    fn buffered_response_and_unterminated_sse_preserve_usage() {
        let wrapped = br#"{"response":{"candidates":[],"usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":2}}}"#;
        let body = response(wrapped).unwrap();
        let usage = crate::executor::usage_from_body(&body);
        assert_eq!((usage.input_tokens, usage.output_tokens), (9, 2));
        let mut decoder = SseDecoder::default();
        assert!(decoder
            .push(&[b"data: ".as_slice(), wrapped].concat(), false)
            .unwrap()
            .is_empty());
        assert!(!decoder.push(&[], true).unwrap().is_empty());
        assert!(response(b"{}").is_err());
    }
}
